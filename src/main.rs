//! hotswap — the swap moment for a mise-installed tool: drain, swap, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{self, Read, pipe},
    iter,
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

#[derive(Clone, Deserialize, Serialize)]
struct Probe {
    code: i32,
    output: String,
    probe: String,
}

#[derive(Default, Deserialize, Serialize)]
struct Record {
    at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<String>,
    /// A rollback onto `version` passed its own steps: red, still a rollback target.
    #[serde(default)]
    held: bool,
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

macro_rules! die {
    ($($a:tt)*) => {{ eprintln!($($a)*); exit(2) }};
}

fn main() {
    // A mise.toml in the caller's cwd would shadow the global pins; every child inherits `/`.
    env::set_current_dir("/").ok();
    let args: Vec<String> = env::args().skip(1).collect();
    let home = env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map_or_else(|| die!("HOME unset"), PathBuf::from);
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let ls = || -> BTreeMap<String, Vec<Install>> {
        let ls = Command::new("mise")
            // Not `--global`: it lists only the active version, so the rollback target would be invisible.
            .args(["ls", "--json"])
            .output()
            .unwrap_or_else(|e| die!("mise: {e}"));
        if !ls.status.success() {
            die!("mise: {}", String::from_utf8_lossy(&ls.stderr).trim());
        }
        serde_json::from_slice(&ls.stdout).unwrap_or_else(|e| die!("mise: {e}"))
    };
    let launchctl = |a: &[&str]| {
        Command::new("launchctl")
            .args(a)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let agent = format!("gui/{uid}/{LABEL}");
    let plist = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    let pl = plist.display();
    let shims = env::var_os("MISE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("mise")))
        .unwrap_or_else(|| home.join(".local/share/mise"))
        .join("shims");
    let (tool, mode) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["setup"] => {
            let dir = env::split_paths(&env::var_os("PATH").unwrap_or_default())
                .find(|d| {
                    fs::metadata(d.join("mise"))
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
                .unwrap_or_else(|| die!("hotswap setup: no mise on PATH"));
            let esc = |p: &PathBuf| {
                let p = p.display().to_string().replace('&', "&amp;");
                p.replace('<', "&lt;").replace('>', "&gt;")
            };
            // a shim re-resolves the globally active hotswap on every exec (launchd's cwd is `/`), so
            // the agent outlives its own swap; no RunAtLoad, since a load-time tick would race the swap that ran setup
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{shims}/hotswap</string>
    <string>tick</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{shims}:{dir}:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StartInterval</key><integer>900</integer>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{home}/Library/Logs/{LABEL}.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/{LABEL}.log</string>
</dict>
</plist>
"#,
                dir = esc(&dir),
                home = esc(&home),
                shims = esc(&shims),
            );
            // hotswap's own swap runs setup from inside the tick, which a bootout would kill
            if fs::read(&plist).is_ok_and(|b| b == xml.as_bytes()) && launchctl(&["print", &agent])
            {
                println!("hotswap setup: {LABEL} unchanged");
                return;
            }
            fs::create_dir_all(home.join("Library/LaunchAgents"))
                .and_then(|()| fs::write(&plist, xml))
                .unwrap_or_else(|e| die!("hotswap setup: {pl}: {e}"));
            launchctl(&["bootout", &agent]);
            if !launchctl(&["bootstrap", &format!("gui/{uid}"), &pl.to_string()]) {
                println!("hotswap setup: launchctl bootstrap gui/{uid} {pl} failed");
                exit(1)
            }
            println!("hotswap setup: {pl} written; {LABEL} loaded, every 900s");
            return;
        }
        ["setup", "--remove"] => {
            let out = ["not loaded", "booted out"][usize::from(launchctl(&["bootout", &agent]))];
            let rm = ["absent", "removed"][usize::from(fs::remove_file(&plist).is_ok())];
            println!("hotswap setup --remove: {LABEL} {out}; {pl} {rm}");
            return;
        }
        ["tick"] => {
            // the shim that ran this tick put the old installs' bin dirs ahead of the shims; shims
            // lead again, so each child `hotswap` is the shim, which re-resolves to the upgraded
            // versions for the swap and its steps
            let path = env::var_os("PATH").unwrap_or_default();
            let path = env::join_paths(iter::once(shims).chain(env::split_paths(&path)))
                .unwrap_or_else(|e| die!("hotswap tick: {e}"));
            let keys: Vec<String> = ls()
                .into_iter()
                // those name a verb, not a tool: a `tick` child would tick again, without bound
                .filter_map(|(k, v)| {
                    let t = k.strip_prefix(GIGANTIC)?;
                    (!["setup", "tick"].contains(&t) && v.iter().any(|i| i.active)).then_some(k)
                })
                .collect();
            if keys.is_empty() {
                println!("hotswap tick: no gigantic tools");
                return;
            }
            // no deadline: mise may be downloading; red or not, versions may already differ
            let up = Command::new("mise")
                .arg("upgrade")
                .args(&keys)
                .status()
                .is_ok_and(|s| s.success());
            if !up {
                println!("hotswap tick: mise upgrade failed");
            }
            // its own group, since a `hotswap setup` bootout of this job kills the job's group and
            // would take the in-flight swap with it
            let red = keys.iter().filter(|k| {
                !Command::new("hotswap")
                    .arg(&k[GIGANTIC.len()..])
                    .env("PATH", &path)
                    .process_group(0)
                    .status()
                    .is_ok_and(|s| s.success())
            });
            exit(i32::from(red.count() > 0 || !up))
        }
        [t] if !t.starts_with('-') => (t, ""),
        [t, m @ ("--back" | "--check")] if !t.starts_with('-') => (t, m),
        _ => die!(
            "usage: hotswap <tool> [--check | --back] | hotswap setup [--remove] | hotswap tick",
        ),
    };
    let Some((key, installs)) = ls()
        .into_iter()
        .find(|(k, v)| k.rsplit([':', '/']).next() == Some(tool) && v.iter().any(|i| i.active))
    else {
        die!("{tool}: no active mise install")
    };
    let [cur] = installs.iter().filter(|i| i.active).collect::<Vec<_>>()[..] else {
        die!("{tool}: several active versions")
    };
    // A version's own manifest; None when mise holds no installed entry for it.
    let manifest = |v: &str| -> Option<Manifest> {
        let i = installs.iter().find(|i| i.version == v && i.installed)?;
        let s = fs::read_to_string(i.install_path.join("hotswap.toml")).unwrap_or_default();
        Some(toml::from_str(&s).unwrap_or_else(|e| die!("{tool}: hotswap.toml: {e}")))
    };
    let m = manifest(&cur.version).unwrap_or_else(|| die!("{tool} {}: not installed", cur.version));
    let dir = home.join(".local/state/hotswap");
    let path = dir.join(format!("{tool}.json"));
    // Held until exit; the kernel drops it if hotswap dies.
    let lock = dir.join(format!("{tool}.lock"));
    let _lock = (mode != "--check").then(|| {
        let f = fs::create_dir_all(&dir)
            .and_then(|()| File::create(&lock))
            .unwrap_or_else(|e| die!("{tool}: {}: {e}", lock.display()));
        f.try_lock()
            .unwrap_or_else(|_| die!("{tool}: another hotswap is running"));
        f
    });
    let old: Option<Record> = serde_json::from_slice(&fs::read(&path).unwrap_or_default()).ok();
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
    // never hotswap's own agent: the next tick finds a new hotswap through its shim, and a kick kills it
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
        let was = old.map_or("none".into(), |r| r.version);
        println!("{tool}: {was} → {}", cur.version);
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
    // Runs a manifest's steps into a record; stops at the first failure.
    let swap = |m: &Manifest, version: String, previous: Option<String>| {
        let steps = steps(m);
        let mut r = Record::default();
        (r.previous, r.version) = (previous, version);
        r.failed = steps.iter().enumerate().find_map(|(i, argv)| {
            let (code, output) = run(m, argv);
            let probe = argv.join(" ");
            if i + m.smoke.len() >= steps.len() {
                r.smoke.push(Probe {
                    code,
                    output,
                    probe: probe.clone(),
                });
            }
            (code != 0).then_some(probe)
        });
        r
    };
    // No deadline: mise may be downloading.
    let mise_use = |v: &str| {
        Command::new("mise")
            .args(["use", "-g", &format!("{key}@{v}")])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    // Stamped, synced and swapped in whole: even a power loss leaves the old record or the new one.
    let save = |r: &mut Record| {
        r.ok = r.failed.is_none();
        r.at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let tmp = path.with_extension("tmp");
        serde_json::to_vec(&r)
            .map_err(io::Error::from)
            .and_then(|b| fs::write(&tmp, b))
            .and_then(|()| File::open(&tmp)?.sync_all())
            .and_then(|()| fs::rename(&tmp, &path))
            .and_then(|()| File::open(&dir)?.sync_all())
            .unwrap_or_else(|e| die!("{tool}: {}: {e}", path.display()));
    };
    let (prev, before, green) = old.map_or((None, None, false), |r| {
        (Some(r.version), r.previous, r.ok || r.held)
    });
    let (mut rec, line) = if mode == "--back" {
        let p = before.unwrap_or_else(|| die!("{tool}: no previous version"));
        let t = manifest(&p).unwrap_or_else(|| die!("{tool} {p}: not installed"));
        if !mise_use(&p) {
            println!("{tool}: mise use {key}@{p} failed");
            exit(1)
        }
        let rec = swap(&t, p, Some(cur.version.clone()));
        let line = rec
            .failed
            .as_ref()
            .map(|f| format!("{tool}: {f} failed on {}; no second rollback", rec.version));
        (rec, line)
    } else {
        let target = prev.as_deref().and_then(manifest);
        for argv in &m.drain {
            if run(&m, argv).0 != 0 {
                println!("{tool}: {} refused; nothing swapped", argv.join(" "));
                exit(1)
            }
        }
        let mut rec = swap(&m, cur.version.clone(), prev.clone());
        // recorded before any rollback: a crash past its mise use then reads red, never the
        // previous version's green on its new exact pin
        save(&mut rec);
        let line = rec.failed.clone().map(|f| {
            let why = match (prev, target) {
                (None, _) => "no previous version".into(),
                (Some(p), _) if !green => format!("{p} is red; no rollback"),
                (Some(p), None) => format!("{p} not installed"),
                (Some(p), Some(_)) if !mise_use(&p) => format!("mise use {key}@{p} failed"),
                (Some(p), Some(t)) => {
                    let back = swap(&t, p, before);
                    rec.smoke.extend(back.smoke);
                    (rec.version, rec.previous, rec.held) =
                        (back.version, back.previous, back.failed.is_none());
                    let again = back.failed.map_or(String::new(), |a| {
                        rec.failed = Some(format!("{f}; after rollback {a}"));
                        format!(", where {a} failed too")
                    });
                    format!("rolled back to {}{again}", rec.version)
                }
            };
            format!("{tool}: {f} failed; {why}")
        });
        (rec, line)
    };
    let n = rec.smoke.len();
    let line = line.unwrap_or_else(|| format!("{tool} {}: green, {n} smoke probes", rec.version));
    save(&mut rec);
    println!("{line}");
    exit(i32::from(!rec.ok))
}
