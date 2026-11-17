//! hotswap — the `upgrade` verb inside each gigantic tool: packslip verifies and unpacks each
//! release into a version dir of hotswap's own, and hotswap owns the moment between — drain,
//! repoint, setup, kickstart, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, pipe},
    os::unix::{
        fs::{PermissionsExt, symlink},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

/// packslip, pinned: its version, then per host (`uname -sm`) the release platform and its
/// sha256, as jdx's install script for that version publishes them; a hotswap change bumps it.
const PACKSLIP: (&str, [[&str; 3]; 3]) = (
    "1.11.4",
    [
        [
            "Darwin arm64",
            "darwin-arm64",
            "2b21dc68d54c23a56123f9f9d7402a8b9cfb1e6950c5d4966730ec5cee20146b",
        ],
        [
            "Linux aarch64",
            "linux-arm64",
            "c0763f4063d5b6d008dd7f3f117126c335861f16980d71895c7f9bec24c4cdd0",
        ],
        [
            "Linux x86_64",
            "linux-x64",
            "bc6211617a25f0ea8c0638cc2295aa9251b7f54c5087105bb46bf44f6357e8c1",
        ],
    ],
);

/// Waits out the upgrade agent's running job, then reloads it from its plist: `$0` the service,
/// `$1` the domain, `$2` the plist.
const RELOAD: &str = r#"while launchctl print "$0" | grep -q "state = running"; do sleep 1; done
launchctl bootout "$0"; launchctl bootstrap "$1" "$2""#;

/// What a tool runs around its swap, and where its releases come from; every field may be empty.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// launchd labels kickstarted once setup ran; the tool's own upgrade agent never is
    pub agents: Vec<String>,
    /// the executable, when it is not named for the tool; empty means the tool's name
    pub bin: String,
    /// run before the swap; a non-zero exit refuses it
    pub drain: Vec<Vec<String>>,
    /// the GitHub repo that releases the tool, `owner/repo`
    pub repo: String,
    /// the tool's own `setup` argv, run after every swap
    pub setup: Vec<String>,
    /// the release signer's fingerprint, `ps1_…` as `packslip install` prints it; every install
    /// requires it
    pub signer: String,
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

/// `$XDG_DATA_HOME/<sub>`, else `~/.local/share/<sub>`.
fn data(sub: &str) -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map_or_else(|| home().join(".local/share"), PathBuf::from)
        .join(sub)
}

/// hotswap's root, `$XDG_DATA_HOME/hotswap`, else `~/.local/share/hotswap`: `<tool>/<version>/`
/// per version, `<tool>/current` the active one, and the pinned packslip with its state.
#[must_use]
pub fn root() -> PathBuf {
    data("hotswap")
}

/// The executable's name: the manifest's `bin`, else the tool's.
fn name<'a>(tool: &'a str, m: &'a Manifest) -> &'a str {
    Some(&*m.bin).filter(|b| !b.is_empty()).unwrap_or(tool)
}

/// The active version: what `<dir>/current` names.
fn current(dir: &Path) -> Option<String> {
    (fs::read_link(dir.join("current")).ok()).map(|p| p.display().to_string())
}

/// Makes `at` a link to `to` by renaming a fresh link over it, so no reader finds it absent.
fn relink(to: &Path, at: &Path) -> io::Result<()> {
    let tmp = at.with_extension("tmp");
    fs::remove_file(&tmp).ok();
    fs::create_dir_all(at.parent().unwrap_or(at))?;
    symlink(to, &tmp).and_then(|()| fs::rename(&tmp, at))
}

/// Whether version `a` is above `b`, compared part by numeric part.
fn newer(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        s.split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect::<Vec<u64>>()
    };
    n(a) > n(b)
}

/// `c`'s stdout, else its reason in one line: packslip's `0:` line, else stderr's last, uncoloured.
fn out(c: &mut Command) -> Result<String, String> {
    let o = c.output().map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&o.stderr);
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).into());
    }
    let why = (err.lines().find_map(|l| l.trim().strip_prefix("0: "))).or(err.lines().last());
    let mut esc = false;
    Err((why.unwrap_or_default().chars())
        .filter(|&c| {
            let keep = !esc && c != '\x1b';
            esc = (esc || c == '\x1b') && c != 'm';
            keep
        })
        .collect())
}

/// This user's id, for the launchd domain `gui/<uid>`.
fn uid() -> String {
    (out(Command::new("id").arg("-u")).unwrap_or_default().trim()).to_owned()
}

/// The pinned packslip under the root, fetched and checked against its sha256 when absent; never
/// one found on PATH.
fn packslip(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let (v, pins) = PACKSLIP;
    let host = out(Command::new("uname").arg("-sm"))?;
    let host = host.trim();
    let &[_, os, sum] = (pins.iter().find(|p| p[0] == host)).ok_or(format!(
        "{host} is unsupported: packslip builds darwin-arm64, linux-arm64 and linux-x64"
    ))?;
    let exe = root.join(format!("packslip/{v}/packslip"));
    if !exe.exists() {
        let (tmp, url) = (
            exe.with_extension("tmp"),
            format!("https://packslip.dev/v{v}/packslip-v{v}-{os}"),
        );
        (|| -> Result<(), Box<dyn Error>> {
            fs::create_dir_all(root.join(format!("packslip/{v}")))?;
            out(Command::new("curl")
                .args(["-fsSL", "-o"])
                .arg(&tmp)
                .arg(&url))?;
            let got = out(Command::new("sha256sum").arg(&tmp))?;
            if got.split(' ').next() != Some(sum) {
                fs::remove_file(&tmp).ok();
                return Err(format!("{url} is not the pinned sha256 {sum}").into());
            }
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            Ok(fs::rename(&tmp, &exe)?)
        })()
        .map_err(|w| format!("packslip {v} bootstrap: {w}"))?;
    }
    Ok(exe)
}

/// Moves a staged tree to `dest` with `<bin>` at its top, a link to `rel` when the release keeps it
/// deeper; a version already at `dest` stays and the stage goes.
fn place(stage: &Path, rel: &Path, bin: &str, dest: &Path) -> io::Result<()> {
    if dest.exists() {
        return fs::remove_dir_all(stage);
    }
    if rel != Path::new(bin) {
        symlink(rel, stage.join(bin))?;
    }
    fs::create_dir_all(dest.parent().unwrap_or(dest)).and_then(|()| fs::rename(stage, dest))
}

/// The empty stage `<root>/stage/<tool>`, whose parent packslip locks.
fn stage(tool: &str) -> io::Result<PathBuf> {
    let s = root().join("stage").join(tool);
    fs::remove_dir_all(&s).ok();
    fs::create_dir_all(root().join("stage")).map(|()| s)
}

/// Installs the release packslip chooses (its latest, withdrawn ones skipped) under the manifest's
/// signer into `<root>/<tool>/<V>/` and returns V; one not newer than `floor` is discarded.
fn fetch(tool: &str, m: &Manifest, floor: &str) -> Result<String, Box<dyn Error>> {
    let root = root();
    let ps = packslip(&root)?;
    let (stage, bins, bin) = (stage(tool)?, root.join("packslip/bin"), name(tool, m));
    fs::create_dir_all(&bins)?;
    // packslip's state, pins and installs live in the root: the user's own packslip use cannot
    // change our trust state; `--bin-dir` keeps its export off `~/.local/bin`, which is ours
    let said = out(Command::new(ps)
        .args([
            "install",
            &format!("github.com/{}", m.repo),
            "--pin",
            &m.signer,
        ])
        .arg("--install-dir")
        .arg(&stage)
        .arg("--bin-dir")
        .arg(&bins)
        .env("XDG_CONFIG_HOME", root.join("packslip/config"))
        .env("XDG_DATA_HOME", root.join("packslip/data"))
        .env("XDG_STATE_HOME", root.join("packslip/state")))
    .map_err(|w| format!("packslip install failed: {w}"))?;
    let v = (said.lines())
        .find_map(|l| l.strip_prefix("Installed ")?.split(' ').nth(1))
        .ok_or("packslip named no version")?;
    if !newer(v, floor) {
        fs::remove_dir_all(&stage)?;
        return Ok(v.into());
    }
    // the export names where the release keeps the binary; the tree moves, so it dangles after
    let rel = (fs::read_link(bins.join(bin)).ok())
        .zip(fs::canonicalize(&stage).ok())
        .and_then(|(t, s)| Some(t.strip_prefix(s).ok()?.to_owned()))
        .ok_or(format!("{} {v} exports no {bin}", m.repo))?;
    place(&stage, &rel, bin, &root.join(tool).join(v))?;
    Ok(v.into())
}

/// Brings the tool into hotswap's layout and returns `~/.local/bin/<bin>`, the path its agents
/// run: the top of the tool's `setup`. Run from mise's install of a version newer than the active
/// one, or with none active, it copies every version mise holds into the layout and makes the
/// running one current — the move off mise, offline, mise's own dirs left alone; with none active
/// and not run from mise, it installs the latest. It then links `~/.local/bin/<bin>` through
/// `current`, and when launchd runs the upgrade agent from another program (mise's shim), it
/// rewrites the agent's plist and reloads it once its running job ends.
///
/// # Errors
/// One line naming the failure.
pub fn ensure(tool: &str, m: &Manifest) -> Result<PathBuf, String> {
    (|| -> Result<PathBuf, Box<dyn Error>> {
        let (dir, bin) = (root().join(tool), name(tool, m));
        let link = home().join(".local/bin").join(bin);
        let cur = current(&dir);
        // the running binary as mise's `installs/<key>/<version>/<rel>`
        let ran = (|| {
            let installs = fs::canonicalize(data("mise").join("installs")).ok()?;
            let at = fs::canonicalize(env::current_exe().ok()?).ok()?;
            let mut c = at.strip_prefix(&installs).ok()?.iter();
            let (key, v) = (installs.join(c.next()?), c.next()?.to_str()?.to_owned());
            Some((key, v, c.as_path().to_owned()))
        })()
        .filter(|(_, v, _)| cur.as_ref().is_none_or(|c| newer(v, c)));
        if let Some((key, v, rel)) = ran {
            for d in fs::read_dir(&key)?.flatten() {
                let n = d.file_name();
                if d.file_type().is_ok_and(|t| t.is_dir())
                    && !n.to_string_lossy().starts_with('.')
                    && !dir.join(&n).exists()
                {
                    let s = stage(tool)?;
                    out(Command::new("cp").arg("-Rp").arg(d.path()).arg(&s))
                        .map_err(|w| format!("cp: {w}"))?;
                    place(&s, &rel, bin, &dir.join(&n))?;
                }
            }
            relink(Path::new(&v), &dir.join("current"))?;
        } else if cur.is_none() {
            let v = fetch(tool, m, "")?;
            relink(Path::new(&v), &dir.join("current"))?;
        }
        relink(&dir.join("current").join(bin), &link)?;
        let label = format!("dev.gigantic.{tool}.upgrade");
        let plist = home().join(format!("Library/LaunchAgents/{label}.plist"));
        let (gui, plist_s) = (format!("gui/{}", uid()), plist.display().to_string());
        let target = format!("{gui}/{label}");
        let said = out(Command::new("launchctl").args(["print", &target])).unwrap_or_default();
        // loaded from this plist, so never a scratch HOME's; launchd re-reads it only on a bootstrap
        if said.contains(&format!("path = {plist_s}"))
            && !said.contains(&format!("program = {}", link.display()))
        {
            fs::write(&plist, agent_plist(tool, bin))?;
            // its own group and no output: it outlives the upgrade run it waits out, and holds no
            // step's pipe open
            (Command::new("/bin/sh").args(["-c", RELOAD, &target, &gui, &plist_s]))
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
        }
        Ok(link)
    })()
    .map_err(|e| format!("{tool}: {e}"))
}

/// A first install, for bifrost or a bare bootstrap: `ensure`, then the manifest's setup through
/// the new link. Returns the exit code: 0 installed, 1 setup failed, 2 unable to install.
#[must_use]
pub fn install(tool: &str, m: &Manifest) -> i32 {
    match ensure(tool, m) {
        Err(e) => {
            eprintln!("{e}");
            2
        }
        Ok(_) => m.setup.split_first().map_or(0, |(n, a)| {
            i32::from(!cmd(n).args(a).status().is_ok_and(|s| s.success()))
        }),
    }
}

/// `n` from `/`, so no project config in the caller's cwd applies, with `~/.local/bin` leading
/// PATH, so each step runs the version `current` names.
fn cmd(n: &str) -> Command {
    let mut path = home().join(".local/bin").into_os_string();
    path.push(":");
    path.push(env::var_os("PATH").unwrap_or_default());
    let mut c = Command::new(n);
    c.current_dir("/").env("PATH", path);
    c
}

/// The launchd plist of the tool's own upgrade agent, `dev.gigantic.<tool>.upgrade`:
/// `~/.local/bin/<bin> upgrade` every 900 s, on a PATH that names no shims and no mise.
#[must_use]
pub fn agent_plist(tool: &str, bin: &str) -> String {
    let home = home().display().to_string().replace('&', "&amp;");
    let home = home.replace('<', "&lt;").replace('>', "&gt;");
    let label = format!("dev.gigantic.{tool}.upgrade");
    // `~/.local/bin/<bin>` resolves through `current` on every exec, so the agent outlives its own
    // swap; no RunAtLoad, since a load-time upgrade would race the swap that ran setup
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{home}/.local/bin/{bin}</string>
    <string>upgrade</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{home}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StartInterval</key><integer>900</integer>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{home}/Library/Logs/{label}.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/{label}.log</string>
</dict>
</plist>
"#
    )
}

/// Drops the tool's record, lock and versions, so a reinstall starts clean; `<tool> setup
/// --remove` calls it.
pub fn forget(tool: &str) {
    let dir = home().join(".local/state/hotswap");
    for e in ["json", "lock"] {
        fs::remove_file(dir.join(format!("{tool}.{e}"))).ok();
    }
    fs::remove_dir_all(root().join(tool)).ok();
}

/// The tool's `upgrade` verb; `args` are the words after it: none, `--check`, `--back` or
/// `--retry` (past a red hold). Returns the process exit code: 0 green or settled, 1 red, 2 unable
/// to try — a failed latest check, bootstrap or install among them, which leaves the active
/// version and the record as they were.
#[must_use]
pub fn upgrade(tool: &str, m: &Manifest, args: &[String]) -> i32 {
    (|| -> Result<i32, String> {
        let bin = name(tool, m);
        let mode = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [] => "",
            [m @ ("--back" | "--check" | "--retry")] => m,
            _ => return Err(format!("usage: {bin} upgrade [--check | --back | --retry]")),
        };
        let home = home();
        if home.as_os_str().is_empty() {
            return Err("HOME unset".into());
        }
        let dir = root().join(tool);
        let cur = current(&dir).ok_or(format!(
            "{tool}: no version under {}; run {bin} setup",
            dir.display()
        ))?;
        let uid = uid();
        let state = home.join(".local/state/hotswap");
        let path = state.join(format!("{tool}.json"));
        // Held until return; the kernel drops it if the process dies.
        let lock = state.join(format!("{tool}.lock"));
        let _lock = (mode != "--check")
            .then(|| {
                let f = fs::create_dir_all(&state).and_then(|()| File::create(&lock));
                let f = f.map_err(|e| format!("{tool}: {}: {e}", lock.display()))?;
                f.try_lock()
                    .map_err(|_| format!("{tool}: another hotswap is running"))?;
                Ok::<_, String>(f)
            })
            .transpose()?;
        let old: Option<Record> = serde_json::from_slice(&fs::read(&path).unwrap_or_default()).ok();
        let mut new = cur.clone();
        if mode != "--back" {
            // the releases/latest redirect names the tag and spends no API quota: the cheap
            // "anything newer?" before packslip, which chooses the version
            let url = format!("https://github.com/{}/releases/latest", m.repo);
            let to = out(cmd("curl")
                .args(["-fsSI", "-o", "/dev/null", "-w", "%{redirect_url}"])
                .arg(&url))
            .map_err(|w| format!("{tool}: {url}: {w}"))?;
            let latest = (to.rsplit_once("/tag/").map(|t| t.1.trim_start_matches('v')))
                .ok_or(format!("{tool}: {url} names no release"))?;
            // a release that went red waits for a newer one, unless retried
            if let Some(r) = (old.as_ref().and_then(|r| r.red.as_ref()))
                .filter(|r| mode != "--retry" && *r == latest)
            {
                println!("{tool}: {r} is red; waiting for a newer release");
                return Ok(i32::from(mode.is_empty()));
            }
            if mode == "--check" && newer(latest, &cur) {
                new = latest.into();
            } else if newer(latest, &cur) {
                let v = fetch(tool, m, &cur).map_err(|e| format!("{tool}: {e}"))?;
                if newer(&v, &cur) {
                    new = v;
                }
            }
        }
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
            .filter(|r| mode != "--back" && r.version == new)
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
            println!("{tool}: {was} → {new}");
            for argv in m.drain.iter().chain(&steps) {
                println!("  {}", argv.join(" "));
            }
            return Ok(0);
        }
        // Repoints `current`, then runs the steps into a record; stops at the first failure.
        let swap = |version: String, previous: Option<String>| {
            let mut r = Record::default();
            (r.previous, r.version) = (previous, version);
            let to = relink(Path::new(&r.version), &dir.join("current"));
            r.failed = to.err().map(|e| format!("current → {}: {e}", r.version));
            r.failed = r.failed.take().or_else(|| {
                steps.iter().enumerate().find_map(|(i, argv)| {
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
                })
            });
            r
        };
        // Stamped, synced and swapped in whole: even a power loss leaves the old record or the new one.
        let save = |r: &mut Record| {
            r.ok = r.failed.is_none();
            // a deliberate rollback holds off the version it left, as a red swap does
            r.red = (mode == "--back" || !r.ok).then(|| new.clone());
            r.at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let tmp = path.with_extension("tmp");
            serde_json::to_vec(&r)
                .map_err(io::Error::from)
                .and_then(|b| fs::write(&tmp, b))
                .and_then(|()| File::open(&tmp)?.sync_all())
                .and_then(|()| fs::rename(&tmp, &path))
                .and_then(|()| File::open(&state)?.sync_all())
                .map_err(|e| format!("{tool}: {}: {e}", path.display()))
        };
        let (prev, before, green) = old.map_or((None, None, false), |r| {
            (Some(r.version), r.previous, r.ok || r.held)
        });
        let (mut rec, line) = if mode == "--back" {
            let p = before.ok_or_else(|| format!("{tool}: no previous version"))?;
            if !dir.join(&p).is_dir() {
                return Err(format!("{tool} {p}: not installed"));
            }
            let rec = swap(p, Some(cur.clone()));
            let line = (rec.failed.as_ref())
                .map(|f| format!("{tool}: {f} failed on {}; no second rollback", rec.version));
            (rec, line)
        } else {
            // drained before `current` moves: a refusal leaves the running version active
            for argv in &m.drain {
                if run(argv).0 != 0 {
                    println!("{tool}: {} refused; nothing swapped", argv.join(" "));
                    return Ok(1);
                }
            }
            let mut rec = swap(new.clone(), prev.clone());
            // recorded before any rollback: a crash past its repoint then reads red, never the
            // previous version's green
            save(&mut rec)?;
            let line = rec.failed.clone().map(|f| {
                let why = match prev {
                    None => "no previous version".into(),
                    Some(p) if !green => format!("{p} is red; no rollback"),
                    Some(p) if !dir.join(&p).is_dir() => format!("{p} not installed"),
                    Some(p) => {
                        // no network: the previous version is still on disk
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
        // green: keep the current version and the previous, the rollback target
        let keep = [
            Some(&*rec.version),
            rec.previous.as_deref(),
            Some("current"),
        ];
        for d in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if rec.ok && !keep.contains(&d.file_name().to_str()) {
                fs::remove_dir_all(d.path()).ok();
            }
        }
        println!("{line}");
        Ok(i32::from(!rec.ok))
    })()
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        2
    })
}
