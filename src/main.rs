//! hotswap — the swap moment for a mise-installed tool: drain, swap, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fmt::Display,
    fs::{self, File},
    io::{self, Read, pipe},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Command, exit},
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Deserialize)]
struct Install {
    active: bool,
    install_path: PathBuf,
    installed: bool,
    version: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Manifest {
    agents: Vec<String>,
    drain: Vec<Vec<String>>,
    setup: Vec<String>,
    smoke: Vec<Vec<String>>,
    timeout: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct Probe {
    code: i32,
    output: String,
    probe: String,
}

#[derive(Deserialize, Serialize)]
struct Record {
    at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<String>,
    smoke: Vec<Probe>,
    version: String,
}

/// The mise key prefix of the gigantic tools, the ones a tick upgrades and swaps.
const GIGANTIC: &str = "packslip:jgeschwendt/";
/// hotswap's own launchd agent.
const LABEL: &str = "dev.gigantic.hotswap";

fn die(msg: impl Display) -> ! {
    eprintln!("{msg}");
    exit(2)
}

fn main() {
    // A mise.toml in the caller's cwd would shadow the global pins; every child inherits `/`.
    env::set_current_dir("/").ok();
    let args: Vec<String> = env::args().skip(1).collect();
    let home = PathBuf::from(env::var_os("HOME").unwrap_or_default());
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let uid = uid.unwrap_or_default();
    let ls = || -> BTreeMap<String, Vec<Install>> {
        let ls = Command::new("mise")
            // Not `--global`: it lists only the active version, so the rollback target would be invisible.
            .args(["ls", "--json"])
            .output()
            .unwrap_or_else(|e| die(format!("mise: {e}")));
        if !ls.status.success() {
            die(format!(
                "mise: {}",
                String::from_utf8_lossy(&ls.stderr).trim()
            ));
        }
        serde_json::from_slice(&ls.stdout).unwrap_or_else(|e| die(format!("mise: {e}")))
    };
    let launchctl = |a: &[&str]| {
        Command::new("launchctl")
            .args(a)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let agent = format!("gui/{uid}/{LABEL}");
    let plist = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let (tool, mode) = match a[..] {
        ["setup"] => {
            let dir = env::split_paths(&env::var_os("PATH").unwrap_or_default())
                .find(|d| {
                    fs::metadata(d.join("mise"))
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
                .unwrap_or_else(|| die("hotswap setup: no mise on PATH"));
            let (d, h) = (dir.display(), home.display());
            // mise x from `/` runs whichever hotswap is globally active, so the agent outlives its own
            // swap; no RunAtLoad, since a load-time tick would race the swap that ran setup for the lock
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{d}/mise</string>
    <string>x</string>
    <string>--</string>
    <string>hotswap</string>
    <string>tick</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{h}</string>
    <key>PATH</key><string>{d}:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StartInterval</key><integer>900</integer>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{h}/Library/Logs/{LABEL}.log</string>
  <key>StandardErrorPath</key><string>{h}/Library/Logs/{LABEL}.log</string>
</dict>
</plist>
"#
            );
            // hotswap's own swap runs setup from inside the tick, which a bootout would kill
            if fs::read(&plist).is_ok_and(|b| b == xml.as_bytes()) && launchctl(&["print", &agent])
            {
                println!("hotswap setup: {LABEL} unchanged");
                return;
            }
            fs::create_dir_all(home.join("Library/LaunchAgents"))
                .and_then(|()| fs::write(&plist, xml))
                .unwrap_or_else(|e| die(format!("hotswap setup: {}: {e}", plist.display())));
            launchctl(&["bootout", &agent]);
            let p = plist.display();
            if !launchctl(&["bootstrap", &format!("gui/{uid}"), &p.to_string()]) {
                println!("hotswap setup: launchctl bootstrap gui/{uid} {p} failed");
                exit(1)
            }
            println!("hotswap setup: {p} written; {LABEL} loaded, every 900s");
            return;
        }
        ["setup", "--remove"] => {
            launchctl(&["bootout", &agent]);
            fs::remove_file(&plist).ok();
            let p = plist.display();
            println!("hotswap setup --remove: {LABEL} booted out; {p} removed");
            return;
        }
        ["tick"] => {
            let keys: Vec<String> = ls()
                .into_iter()
                .filter(|(k, v)| k.starts_with(GIGANTIC) && v.iter().any(|i| i.active))
                .map(|(k, _)| k)
                .collect();
            if keys.is_empty() {
                println!("hotswap tick: no gigantic tools");
                return;
            }
            // no deadline: mise may be downloading; red or not, versions may already differ
            if !Command::new("mise")
                .arg("upgrade")
                .args(&keys)
                .status()
                .is_ok_and(|s| s.success())
            {
                println!("hotswap tick: mise upgrade failed");
            }
            let exe = env::current_exe().unwrap_or_default();
            let red = keys.iter().filter(|k| {
                !Command::new(&exe)
                    .arg(&k[GIGANTIC.len()..])
                    .status()
                    .is_ok_and(|s| s.success())
            });
            exit(i32::from(red.count() > 0))
        }
        [t] if !t.starts_with('-') => (t, ""),
        [t, m @ ("--back" | "--check")] if !t.starts_with('-') => (t, m),
        _ => die(
            "usage: hotswap <tool> [--check | --back] | hotswap setup [--remove] | hotswap tick",
        ),
    };
    let Some((key, installs)) = ls()
        .into_iter()
        .find(|(k, v)| k.rsplit([':', '/']).next() == Some(tool) && v.iter().any(|i| i.active))
    else {
        die(format!("{tool}: no active mise install"))
    };
    let [cur] = installs.iter().filter(|i| i.active).collect::<Vec<_>>()[..] else {
        die(format!("{tool}: several active versions"))
    };
    // A version's own manifest; None when mise holds no installed entry for it.
    let manifest = |v: &str| -> Option<Manifest> {
        let i = installs.iter().find(|i| i.version == v && i.installed)?;
        let s = fs::read_to_string(i.install_path.join("hotswap.toml")).unwrap_or_default();
        Some(toml::from_str(&s).unwrap_or_else(|e| die(format!("{tool}: hotswap.toml: {e}"))))
    };
    let m = manifest(&cur.version)
        .unwrap_or_else(|| die(format!("{tool} {}: not installed", cur.version)));
    let dir = home.join(".local/state/hotswap");
    let path = dir.join(format!("{tool}.json"));
    // Held until exit; the kernel drops it if hotswap dies.
    let lock = dir.join(format!("{tool}.lock"));
    let _lock = (mode != "--check").then(|| {
        let f = fs::create_dir_all(&dir)
            .and_then(|()| File::create(&lock))
            .unwrap_or_else(|e| die(format!("{tool}: {}: {e}", lock.display())));
        f.try_lock()
            .unwrap_or_else(|_| die(format!("{tool}: another hotswap is running")));
        f
    });
    let old: Option<Record> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    // Settled on this version, green or red: only a new version or `--back` moves it.
    if let Some(r) = old
        .as_ref()
        .filter(|r| mode != "--back" && r.version == cur.version)
    {
        let state = if r.ok {
            "up to date".into()
        } else {
            format!("red at {}", r.failed.as_deref().unwrap_or_default())
        };
        println!("{tool} {}: {state}", r.version);
        exit(i32::from(!r.ok && mode.is_empty()))
    }
    // Setup, then the kickstarts (a new agent is loaded only once setup ran), then the smoke probes.
    // never hotswap's own agent: the next tick finds a new hotswap through mise x, and a kick kills it
    let steps = |m: &Manifest| -> Vec<Vec<String>> {
        let kick = m.agents.iter().filter(|a| *a != LABEL).map(|a| {
            ["launchctl", "kickstart", "-k", &format!("gui/{uid}/{a}")]
                .map(String::from)
                .into()
        });
        (!m.setup.is_empty())
            .then(|| m.setup.clone())
            .into_iter()
            .chain(kick)
            .chain(m.smoke.clone())
            .collect()
    };
    if mode == "--check" {
        println!(
            "{tool}: {} → {}",
            old.map_or("none".into(), |r| r.version),
            cur.version
        );
        for argv in m.drain.iter().chain(&steps(&m)) {
            println!("  {}", argv.join(" "));
        }
        return;
    }
    // One pipe for stdout and stderr, drained on a thread while the deadline is polled.
    let run = |m: &Manifest, argv: &[String]| {
        let secs = m.timeout.unwrap_or(120);
        // Capped: an absurd `timeout` would overflow the Instant and panic on every tick.
        let end = Instant::now() + Duration::from_secs(secs.min(86_400));
        (|| {
            let (mut rx, tx) = pipe()?;
            let mut child = Command::new(argv.first().unwrap_or(&String::new()))
                .args(argv.iter().skip(1))
                .stdout(tx.try_clone()?)
                .stderr(tx)
                // Its own group, so the kill on expiry reaches whatever the step started too.
                .process_group(0)
                .spawn()?;
            let out = thread::spawn(move || {
                let mut buf = vec![];
                rx.read_to_end(&mut buf).map(|_| buf)
            });
            // The pipe can outlive the child: a step that backgrounds a process hands it the write end.
            let mut code = None;
            while code.is_none() || !out.is_finished() {
                code = code.or(child.try_wait()?.map(|s| s.code().unwrap_or(-1)));
                if Instant::now() > end {
                    return code.map_or_else(
                        || {
                            Command::new("kill")
                                .args(["-KILL", "--", &format!("-{}", child.id())])
                                .status()
                                .map(|_| (-1, format!("timed out after {secs}s")))
                        },
                        |c| Ok((c, format!("output held open past {secs}s"))),
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            let buf = out.join().unwrap_or(Ok(vec![]))?;
            Ok((code.unwrap_or(-1), String::from_utf8_lossy(&buf).into()))
        })()
        .unwrap_or_else(|e: io::Error| (-1, e.to_string()))
    };
    // Runs a manifest's steps; stops at the first failure.
    let swap = |m: &Manifest| {
        let steps = steps(m);
        let mut smoke = vec![];
        for (i, argv) in steps.iter().enumerate() {
            let (code, output) = run(m, argv);
            let probe = argv.join(" ");
            if i + m.smoke.len() >= steps.len() {
                smoke.push(Probe {
                    code,
                    output,
                    probe: probe.clone(),
                });
            }
            if code != 0 {
                return (Some(probe), smoke);
            }
        }
        (None, smoke)
    };
    // No deadline: mise may be downloading.
    let mise_use = |v: &str| {
        Command::new("mise")
            .args(["use", "-g", &format!("{key}@{v}")])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let (prev, before) = old.map_or((None, None), |r| (Some(r.version), r.previous));
    let (version, previous, failed, smoke, line) = if mode == "--back" {
        let p = before.unwrap_or_else(|| die(format!("{tool}: no previous version")));
        let t = manifest(&p).unwrap_or_else(|| die(format!("{tool} {p}: not installed")));
        if !mise_use(&p) {
            println!("{tool}: mise use {key}@{p} failed");
            exit(1)
        }
        let (failed, smoke) = swap(&t);
        let line = failed
            .as_ref()
            .map(|f| format!("{tool}: {f} failed on {p}; no second rollback"));
        (p, Some(cur.version.clone()), failed, smoke, line)
    } else {
        let target = prev.as_deref().and_then(manifest);
        for argv in &m.drain {
            if run(&m, argv).0 != 0 {
                println!("{tool}: {} refused; nothing swapped", argv.join(" "));
                exit(1)
            }
        }
        let (mut failed, mut smoke) = swap(&m);
        let (mut version, mut previous) = (cur.version.clone(), prev.clone());
        let line = failed.clone().map(|f| {
            let why = match (prev, target) {
                (None, _) => "no previous version".into(),
                (Some(p), None) => format!("{p} not installed"),
                (Some(p), Some(_)) if !mise_use(&p) => format!("mise use {key}@{p} failed"),
                (Some(p), Some(t)) => {
                    let (again, rest) = swap(&t);
                    smoke.extend(rest);
                    (version, previous) = (p, before);
                    failed = Some(
                        again
                            .as_ref()
                            .map_or(f.clone(), |a| format!("{f}; after rollback {a}")),
                    );
                    let again = again.map_or(String::new(), |a| format!(", where {a} failed too"));
                    format!("rolled back to {version}{again}")
                }
            };
            format!("{tool}: {f} failed; {why}")
        });
        (version, previous, failed, smoke, line)
    };
    let code = i32::from(failed.is_some());
    let line =
        line.unwrap_or_else(|| format!("{tool} {version}: green, {} smoke probes", smoke.len()));
    let at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let record = Record {
        at,
        ok: failed.is_none(),
        failed,
        previous,
        smoke,
        version,
    };
    let tmp = path.with_extension("tmp");
    serde_json::to_vec(&record)
        .map_err(io::Error::from)
        .and_then(|b| fs::write(&tmp, b))
        .and_then(|()| fs::rename(&tmp, &path))
        .unwrap_or_else(|e| die(format!("{tool}: {}: {e}", path.display())));
    println!("{line}");
    exit(code)
}
