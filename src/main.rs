//! hotswap — the swap moment for a mise-installed tool: drain, swap, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fmt::Display,
    fs::{self, File},
    io::{self, Read, pipe},
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

fn die(msg: impl Display) -> ! {
    eprintln!("{msg}");
    exit(2)
}

fn main() {
    // A mise.toml in the caller's cwd would shadow the global pins; every child inherits `/`.
    env::set_current_dir("/").ok();
    let args: Vec<String> = env::args().skip(1).collect();
    let (tool, mode) = match &args[..] {
        [t] if !t.starts_with('-') => (t, ""),
        [t, m] if !t.starts_with('-') && (m == "--back" || m == "--check") => (t, m.as_str()),
        _ => die("usage: hotswap <tool> [--check | --back]"),
    };
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
    let tools: BTreeMap<String, Vec<Install>> =
        serde_json::from_slice(&ls.stdout).unwrap_or_else(|e| die(format!("mise: {e}")));
    let Some((key, installs)) = tools
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
    let dir = PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/state/hotswap");
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
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let uid = uid.unwrap_or_default();
    // Setup, then the kickstarts (a new agent is loaded only once setup ran), then the smoke probes.
    let steps = |m: &Manifest| -> Vec<Vec<String>> {
        let kick = m.agents.iter().map(|a| {
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
        let end = Instant::now() + Duration::from_secs(secs);
        (|| {
            let (mut rx, tx) = pipe()?;
            let mut child = Command::new(argv.first().unwrap_or(&String::new()))
                .args(argv.iter().skip(1))
                .stdout(tx.try_clone()?)
                .stderr(tx)
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
                            child
                                .kill()
                                .map(|()| (-1, format!("timed out after {secs}s")))
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
