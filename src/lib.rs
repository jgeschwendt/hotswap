//! hotswap — the `upgrade` verb inside each gigantic tool: mise moves the version, hotswap the moment
//! between — drain, setup, kickstart, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{self, Read, pipe},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Deserialize)]
struct Install {
    active: bool,
    installed: bool,
    version: String,
}

/// What a tool runs around its swap; every field may be empty.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// launchd labels kickstarted once setup ran; the tool's own upgrade agent never is
    pub agents: Vec<String>,
    /// the executable, when it is not named for the tool; empty means the tool's name
    pub bin: String,
    /// run before the swap; a non-zero exit refuses it
    pub drain: Vec<Vec<String>>,
    /// the tool's own `setup` argv, run after every swap
    pub setup: Vec<String>,
    /// probes run against the new processes
    pub smoke: Vec<Vec<String>>,
    /// seconds per step, 120 when unset; expiry kills the step's process group
    pub timeout: Option<u64>,
}

#[derive(Deserialize, Serialize)]
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
    /// The version this run failed on or rolled back from: an upgrade waits while it is latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    red: Option<String>,
    smoke: Vec<Probe>,
    version: String,
}

/// `$HOME`, empty when unset.
fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default())
}

/// mise's shims dir: under `MISE_DATA_DIR`, else `XDG_DATA_HOME/mise`, else `~/.local/share/mise`.
#[must_use]
pub fn shims() -> PathBuf {
    env::var_os("MISE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("mise")))
        .unwrap_or_else(|| home().join(".local/share/mise"))
        .join("shims")
}

/// The launchd plist of the tool's own upgrade agent, `dev.gigantic.<tool>.upgrade`: `<bin> upgrade`
/// through mise's shim every 900 s.
#[must_use]
pub fn agent_plist(tool: &str, bin: &str) -> String {
    let (home, shims) = (home(), shims());
    let esc = |p: &PathBuf| {
        let p = p.display().to_string().replace('&', "&amp;");
        p.replace('<', "&lt;").replace('>', "&gt;")
    };
    // the shims hold no mise for `upgrade` to call; mise's installer default when PATH has none
    let mise = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .find(|d| {
            fs::metadata(d.join("mise"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| home.join(".local/bin"));
    let label = format!("dev.gigantic.{tool}.upgrade");
    // a shim re-resolves the globally active version on every exec (launchd's cwd is `/`), so the
    // agent outlives its own swap; no RunAtLoad, since a load-time upgrade would race the swap that
    // ran setup
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{shims}/{bin}</string>
    <string>upgrade</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{shims}:{mise}:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StartInterval</key><integer>900</integer>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{home}/Library/Logs/{label}.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/{label}.log</string>
</dict>
</plist>
"#,
        home = esc(&home),
        mise = esc(&mise),
        shims = esc(&shims),
    )
}

/// Drops the tool's record and lock, so a reinstall starts clean; `<tool> setup --remove` calls it.
pub fn forget(tool: &str) {
    let dir = home().join(".local/state/hotswap");
    for e in ["json", "lock"] {
        fs::remove_file(dir.join(format!("{tool}.{e}"))).ok();
    }
}

/// The tool's `upgrade` verb; `args` are the words after it: none, `--check`, `--back` or
/// `--retry` (past a red hold). Returns the process exit code: 0 green or settled, 1 red, 2 unable
/// to try.
#[must_use]
pub fn upgrade(tool: &str, m: &Manifest, args: &[String]) -> i32 {
    (|| -> Result<i32, String> {
        let mode = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [] => "",
            [m @ ("--back" | "--check" | "--retry")] => m,
            _ => {
                let bin = Some(&*m.bin).filter(|b| !b.is_empty()).unwrap_or(tool);
                return Err(format!("usage: {bin} upgrade [--check | --back | --retry]"));
            }
        };
        let home = home();
        if home.as_os_str().is_empty() {
            return Err("HOME unset".into());
        }
        // a shim puts the running install's bin dir ahead of the shims; shims lead again, so each
        // step resolves the version mise just made active
        let mut path = shims().into_os_string();
        path.push(":");
        path.push(env::var_os("PATH").unwrap_or_default());
        // from `/`: a mise.toml in the caller's cwd would shadow the global pins
        let cmd = |n: &str| {
            let mut c = Command::new(n);
            c.current_dir("/").env("PATH", &path);
            c
        };
        let uid = (cmd("id").arg("-u").output())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        // the mise key, its active version and every installed one
        let find = || -> Result<(String, String, Vec<String>), String> {
            // Not `--global`: it lists only the active version, so the rollback target would be invisible.
            let o =
                (cmd("mise").args(["ls", "--json"]).output()).map_err(|e| format!("mise: {e}"))?;
            if !o.status.success() {
                return Err(format!(
                    "mise: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
            let all: BTreeMap<String, Vec<Install>> =
                serde_json::from_slice(&o.stdout).map_err(|e| format!("mise: {e}"))?;
            // a key naming the tool on any backend, holding an active version
            let (key, v) = (all.into_iter())
                .find(|(k, v)| {
                    k.rsplit([':', '/']).next() == Some(tool) && v.iter().any(|i| i.active)
                })
                .ok_or_else(|| format!("{tool}: no active mise install"))?;
            let [cur] = v.iter().filter(|i| i.active).collect::<Vec<_>>()[..] else {
                return Err(format!("{tool}: several active versions"));
            };
            let installed = v.iter().filter(|i| i.installed).map(|i| i.version.clone());
            Ok((key, cur.version.clone(), installed.collect()))
        };
        // mise's last line that is not its own chatter
        let last = |b: &[u8]| {
            (String::from_utf8_lossy(b).lines())
                .rfind(|l| ![" WARN ", "--verbose"].iter().any(|n| l.contains(n)))
                .map(str::to_owned)
        };
        // No deadline: mise may be downloading. Err is mise's reason as a `: why` suffix, empty
        // when it printed none.
        let mise = |a: &[&str]| match cmd("mise").args(a).output() {
            Ok(o) if o.status.success() => Ok(()),
            o => Err((o.ok().and_then(|o| last(&o.stderr)))
                .map(|w| format!(": {w}"))
                .unwrap_or_default()),
        };
        let found = find()?;
        let dir = home.join(".local/state/hotswap");
        let path = dir.join(format!("{tool}.json"));
        // Held until return; the kernel drops it if the process dies.
        let lock = dir.join(format!("{tool}.lock"));
        let _lock = (mode != "--check")
            .then(|| {
                let f = fs::create_dir_all(&dir).and_then(|()| File::create(&lock));
                let f = f.map_err(|e| format!("{tool}: {}: {e}", lock.display()))?;
                f.try_lock()
                    .map_err(|_| format!("{tool}: another hotswap is running"))?;
                Ok::<_, String>(f)
            })
            .transpose()?;
        let old: Option<Record> = serde_json::from_slice(&fs::read(&path).unwrap_or_default()).ok();
        // a release that went red waits for a newer one, unless retried
        if matches!(mode, "" | "--check") {
            let latest = (cmd("mise").args(["latest", &found.0]).output().ok())
                .filter(|o| o.status.success())
                .and_then(|o| last(&o.stdout));
            if let Some(r) =
                (old.as_ref().and_then(|r| r.red.as_ref())).filter(|r| latest.as_ref() == Some(r))
            {
                println!("{tool}: {r} is red; waiting for a newer release");
                return Ok(i32::from(mode.is_empty()));
            }
        }
        // --bump moves the exact pin a rollback set
        let (key, cur, installed) = if matches!(mode, "" | "--retry") {
            if let Err(w) = mise(&["upgrade", "--bump", &found.0]) {
                println!("{tool}: mise upgrade failed{w}");
                return Ok(1);
            }
            find()?
        } else {
            found
        };
        // One pipe for stdout and stderr, drained on a thread while the deadline is polled.
        let run = |argv: &[String]| {
            let secs = m.timeout.unwrap_or(120);
            // Capped: an absurd `timeout` would overflow the Instant and panic.
            let end = Instant::now() + Duration::from_secs(secs.min(86_400));
            (|| {
                let (mut rx, tx) = pipe()?;
                let mut child = cmd(argv.first().map_or("", String::as_str))
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
                                (cmd("kill").args(["-KILL", "--", &format!("-{}", child.id())]))
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
        // Settled on this version, green or red: only a new version or `--back` moves it.
        if let Some(r) = old
            .as_ref()
            .filter(|r| mode != "--back" && r.version == cur)
        {
            let state = if r.ok {
                "up to date".into()
            } else {
                format!("red at {}", r.failed.as_deref().unwrap_or_default())
            };
            println!("{tool} {}: {state}", r.version);
            return Ok(i32::from(!r.ok && mode != "--check"));
        }
        // Setup, then the kickstarts (a new agent is loaded only once setup ran), then the smoke
        // probes; never the tool's own upgrade agent, whose kick would kill this very swap.
        let me = format!("dev.gigantic.{tool}.upgrade");
        let kick = m.agents.iter().filter(|a| **a != me).map(|a| {
            ["launchctl", "kickstart", "-k", &format!("gui/{uid}/{a}")]
                .map(String::from)
                .into()
        });
        let steps: Vec<Vec<String>> = (!m.setup.is_empty())
            .then(|| m.setup.clone())
            .into_iter()
            .chain(kick)
            .chain(m.smoke.clone())
            .collect();
        if mode == "--check" {
            let was = old.map_or("none".into(), |r| r.version);
            println!("{tool}: {was} → {cur}");
            for argv in m.drain.iter().chain(&steps) {
                println!("  {}", argv.join(" "));
            }
            return Ok(0);
        }
        // Runs the steps into a record; stops at the first failure.
        let swap = |version: String, previous: Option<String>| {
            let mut r = Record::default();
            (r.previous, r.version) = (previous, version);
            r.failed = steps.iter().enumerate().find_map(|(i, argv)| {
                let (code, output) = run(argv);
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
        // Stamped, synced and swapped in whole: even a power loss leaves the old record or the new one.
        let save = |r: &mut Record| {
            r.ok = r.failed.is_none();
            // a deliberate rollback holds off the version it left, as a red swap does
            r.red = (mode == "--back" || !r.ok).then(|| cur.clone());
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
                .map_err(|e| format!("{tool}: {}: {e}", path.display()))
        };
        let (prev, before, green) = old.map_or((None, None, false), |r| {
            (Some(r.version), r.previous, r.ok || r.held)
        });
        let (mut rec, line) = if mode == "--back" {
            let p = before.ok_or_else(|| format!("{tool}: no previous version"))?;
            if !installed.contains(&p) {
                return Err(format!("{tool} {p}: not installed"));
            }
            if let Err(w) = mise(&["use", "-g", &format!("{key}@{p}")]) {
                println!("{tool}: mise use {key}@{p} failed{w}");
                return Ok(1);
            }
            let rec = swap(p, Some(cur.clone()));
            let line = (rec.failed.as_ref())
                .map(|f| format!("{tool}: {f} failed on {}; no second rollback", rec.version));
            (rec, line)
        } else {
            for argv in &m.drain {
                if run(argv).0 != 0 {
                    println!("{tool}: {} refused; nothing swapped", argv.join(" "));
                    return Ok(1);
                }
            }
            let mut rec = swap(cur.clone(), prev.clone());
            // recorded before any rollback: a crash past its mise use then reads red, never the
            // previous version's green on its new exact pin
            save(&mut rec)?;
            let line = rec.failed.clone().map(|f| {
                let why = match prev {
                    None => "no previous version".into(),
                    Some(p) if !green => format!("{p} is red; no rollback"),
                    Some(p) if !installed.contains(&p) => format!("{p} not installed"),
                    Some(p) => {
                        if let Err(w) = mise(&["use", "-g", &format!("{key}@{p}")]) {
                            return format!("{tool}: {f} failed; mise use {key}@{p} failed{w}");
                        }
                        let back = swap(p, before);
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
        let line =
            line.unwrap_or_else(|| format!("{tool} {}: green, {n} smoke probes", rec.version));
        save(&mut rec)?;
        println!("{line}");
        Ok(i32::from(!rec.ok))
    })()
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        2
    })
}
