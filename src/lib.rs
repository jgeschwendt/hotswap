//! hotswap — the `upgrade` verb inside each gigantic tool: packslip verifies and unpacks each
//! release into a version dir of hotswap's own, and hotswap owns the moment between — drain,
//! repoint, setup, kickstart, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, Write, pipe},
    os::unix::{
        fs::{PermissionsExt, symlink},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

/// packslip, pinned: its version, then per host (`uname -sm`) the release platform, the hash
/// command the host ships (`shasum` on every macOS, coreutils' `sha256sum` on Linux) and the sha256
/// jdx's install script for that version publishes; a hotswap change bumps it.
const PACKSLIP: (&str, [[&str; 4]; 3]) = (
    "1.11.4",
    [
        [
            "Darwin arm64",
            "darwin-arm64",
            "shasum -a 256",
            "2b21dc68d54c23a56123f9f9d7402a8b9cfb1e6950c5d4966730ec5cee20146b",
        ],
        [
            "Linux aarch64",
            "linux-arm64",
            "sha256sum",
            "c0763f4063d5b6d008dd7f3f117126c335861f16980d71895c7f9bec24c4cdd0",
        ],
        [
            "Linux x86_64",
            "linux-x64",
            "sha256sum",
            "bc6211617a25f0ea8c0638cc2295aa9251b7f54c5087105bb46bf44f6357e8c1",
        ],
    ],
);

/// Waits out the upgrade agent's running job, boots it out, waits (30 s at most) for launchd to let
/// go — a bootstrap in that window fails — then bootstraps it from its plist, five tries 2 s apart,
/// printing one line on the outcome; its pid file goes, or reads `unloaded` for the next `ensure`
/// to load: `$0` the service, `$1` the domain, `$2` the plist, `$3` the pid file.
const RELOAD: &str = r#"while launchctl print "$0" 2>/dev/null | grep -q "state = running"; do sleep 1; done
launchctl bootout "$0" >/dev/null 2>&1
i=0; while launchctl print "$0" >/dev/null 2>&1 && [ $i -lt 30 ]; do i=$((i+1)); sleep 1; done
i=0; until e=$(launchctl bootstrap "$1" "$2" 2>&1); do
  i=$((i+1)); [ $i -ge 5 ] && { echo "hotswap: $0 not reloaded from $2 after $i tries: $(echo "$e" | head -1)"; echo unloaded > "$3"; exit 1; }; sleep 2
done
echo "hotswap: $0 reloaded from $2"; rm -f "$3""#;

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
    /// seconds a `packslip install` may take, 600 when unset; expiry kills its process group
    pub fetch_timeout: Option<u64>,
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

/// `$HOME`, refused when unset: a relative path would write into the cwd.
fn homed() -> Result<PathBuf, String> {
    Some(home())
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or("HOME unset".into())
}

/// hotswap's root, always `~/.local/share/hotswap`, so a shell, a launchd agent and bifrost agree:
/// `<tool>/<version>/` per version, `<tool>/current` the active one, and the pinned packslip with
/// its state.
#[must_use]
pub fn root() -> PathBuf {
    home().join(".local/share/hotswap")
}

/// mise's data dir, to find what a mise install left: `MISE_DATA_DIR`, else `XDG_DATA_HOME/mise`,
/// else `~/.local/share/mise`.
fn mise() -> PathBuf {
    (env::var_os("MISE_DATA_DIR").map(PathBuf::from))
        .or_else(|| env::var_os("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("mise")))
        .unwrap_or_else(|| home().join(".local/share/mise"))
}

/// The executable's name: the manifest's `bin`, else the tool's.
fn name<'a>(tool: &'a str, m: &'a Manifest) -> &'a str {
    Some(&*m.bin).filter(|b| !b.is_empty()).unwrap_or(tool)
}

/// The active version: what `<dir>/current` names, when that version is on disk.
fn current(dir: &Path) -> Option<String> {
    (fs::read_link(dir.join("current")).ok())
        .filter(|v| dir.join(v).is_dir())
        .map(|p| p.display().to_string())
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
fn out(c: &mut Command) -> Result<String, Box<dyn Error>> {
    let o = c.output()?;
    let err = String::from_utf8_lossy(&o.stderr);
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).into());
    }
    Err(reason(&err, o.status.signal()).into())
}

/// A failure's reason in one line: packslip's `0:` line, else the last line, else the signal that
/// killed it, uncoloured.
fn reason(err: &str, sig: Option<i32>) -> String {
    let sig = sig.map(|n| format!("killed by signal {n}"));
    let why = (err.lines().find_map(|l| l.trim().strip_prefix("0: ")))
        .or(err.lines().last())
        .or(sig.as_deref());
    let mut esc = false;
    (why.unwrap_or_default().chars())
        .filter(|&c| {
            let keep = !esc && c != '\x1b';
            esc = (esc || c == '\x1b') && c != 'm';
            keep
        })
        .collect()
}

/// Runs `c` in its own process group, stdout and stderr on one pipe drained on a thread while the
/// deadline is polled; past `secs` the group is killed. Returns the exit code (-1 for a signal, the
/// deadline or no spawn), the output, and the signal.
fn run(mut c: Command, secs: u64) -> (i32, String, Option<i32>) {
    // capped: an absurd deadline would overflow the Instant and panic
    let end = Instant::now() + Duration::from_secs(secs.min(86_400));
    (|| {
        let (mut rx, tx) = pipe()?;
        let spawned = (c.stdout(tx.try_clone()?).stderr(tx))
            .process_group(0)
            .spawn();
        // the Command holds the pipe's write ends until dropped
        drop(c);
        let mut child = spawned?;
        let out = thread::spawn(move || {
            let mut buf = vec![];
            rx.read_to_end(&mut buf).map(|_| buf)
        });
        // the pipe can outlive the child: a step that backgrounds a process hands it the write end
        let mut status = None;
        while status.is_none() || !out.is_finished() {
            status = status.or(child.try_wait()?);
            if Instant::now() > end {
                return status.map_or_else(
                    || {
                        (Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]))
                            .status()
                            .map(|_| (-1, format!("timed out after {secs}s"), None))
                    },
                    |s: process::ExitStatus| {
                        let held = format!("output held open past {secs}s");
                        Ok((s.code().unwrap_or(-1), held, s.signal()))
                    },
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        let (buf, s) = (
            out.join().unwrap_or(Ok(vec![]))?,
            status.unwrap_or_default(),
        );
        Ok((
            s.code().unwrap_or(-1),
            String::from_utf8_lossy(&buf).into(),
            s.signal(),
        ))
    })()
    .unwrap_or_else(|e: io::Error| (-1, e.to_string(), None))
}

/// Writes `b` to `path` whole: a synced temp renamed over it, the dir synced, so even a power loss
/// leaves the old file or the new one.
fn persist(path: &Path, b: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let (tmp, dir) = (PathBuf::from(tmp), path.parent().unwrap_or(path));
    fs::write(&tmp, b)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// This user's id, for the launchd domain `gui/<uid>`.
fn uid() -> String {
    (out(Command::new("id").arg("-u")).unwrap_or_default().trim()).to_owned()
}

/// The pinned packslip under the root, its sha256 checked before each use and fetched again on a
/// mismatch; never one found on PATH.
fn packslip(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let (v, pins) = PACKSLIP;
    let host = out(Command::new("uname").arg("-sm"))?;
    let host = host.trim();
    let &[_, os, hash, sum] = (pins.iter().find(|p| p[0] == host)).ok_or(format!(
        "{host} is unsupported: packslip builds darwin-arm64, linux-arm64 and linux-x64"
    ))?;
    let (dir, url) = (
        root.join(format!("packslip/{v}")),
        format!("https://packslip.dev/v{v}/packslip-v{v}-{os}"),
    );
    let exe = dir.join("packslip");
    let pinned = |p: &Path| {
        let mut a = hash.split(' ');
        out(Command::new(a.next().unwrap_or_default()).args(a).arg(p))
            .map(|o| o.split(' ').next() == Some(sum))
    };
    if !pinned(&exe).unwrap_or(false) {
        // a name of its own: a concurrent bootstrap never writes into the file this one checks
        let tmp = dir.join(format!("packslip.{}.tmp", process::id()));
        (|| -> Result<(), Box<dyn Error>> {
            fs::create_dir_all(&dir)?;
            out(Command::new("curl")
                .args(["-q", "-fsSL", "-o"])
                .arg(&tmp)
                .arg(&url))?;
            if !pinned(&tmp)? {
                return Err(format!("{url} is not the pinned sha256 {sum}").into());
            }
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            File::open(&tmp)?.sync_all()?;
            fs::rename(&tmp, &exe)?;
            Ok(File::open(&dir)?.sync_all()?)
        })()
        .map_err(|w| {
            fs::remove_file(&tmp).ok();
            format!("packslip {v} bootstrap: {w}")
        })?;
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

/// The tool's own packslip dirs, `<root>/packslip/<tool>/`: its `state`, `config`, `data`, `bin`
/// and `stage`, which packslip locks for a whole install, so no tool's install waits on another's;
/// only the pinned binary is shared.
fn own(tool: &str) -> PathBuf {
    root().join("packslip").join(tool)
}

/// The empty stage `<root>/packslip/<tool>/stage`; a leftover that will not go is an error naming
/// it, never a tree a copy nests into.
fn stage(tool: &str) -> io::Result<PathBuf> {
    let s = own(tool).join("stage");
    // a copy keeps its modes, so a leftover can hold a dir its owner cannot list
    if fs::remove_dir_all(&s).is_err() && s.exists() {
        Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&s)
            .status()?;
        fs::remove_dir_all(&s).map_err(|e| io::Error::other(format!("{}: {e}", s.display())))?;
    }
    fs::create_dir_all(own(tool)).map(|()| s)
}

/// Installs the release packslip chooses (its latest, withdrawn ones skipped) under the manifest's
/// signer into `<root>/<tool>/<V>/` and returns V; one not newer than `floor` is discarded, and a
/// failure leaves no stage.
fn fetch(tool: &str, m: &Manifest, floor: &str) -> Result<String, Box<dyn Error>> {
    let (root, own, bin) = (root(), own(tool), name(tool, m));
    let ps = packslip(&root)?;
    let stage = stage(tool)?;
    let got = (|| -> Result<String, Box<dyn Error>> {
        let bins = own.join("bin");
        fs::create_dir_all(&bins)?;
        // packslip's state, pins and installs are the tool's own: neither the user's packslip use
        // nor another tool's install can change or block ours; `--bin-dir` keeps its export off
        // `~/.local/bin`, which is ours
        let mut c = Command::new(ps);
        (c.args([
            "install",
            &format!("github.com/{}", m.repo),
            "--pin",
            &m.signer,
        ]))
        .arg("--install-dir")
        .arg(&stage)
        .arg("--bin-dir")
        .arg(&bins)
        .env("XDG_CONFIG_HOME", own.join("config"))
        .env("XDG_DATA_HOME", own.join("data"))
        .env("XDG_STATE_HOME", own.join("state"));
        let (code, said, sig) = run(c, m.fetch_timeout.unwrap_or(600));
        if code != 0 {
            return Err(format!("packslip install failed: {}", reason(&said, sig)).into());
        }
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
    })();
    if got.is_err() {
        fs::remove_dir_all(&stage).ok();
    }
    got
}

/// The running binary as mise's `installs/<key>/<version>/<rel>`: the key's dir, the version and
/// the rel.
fn from_mise() -> Option<(PathBuf, String, PathBuf)> {
    let installs = fs::canonicalize(mise().join("installs")).ok()?;
    let at = fs::canonicalize(env::current_exe().ok()?).ok()?;
    let mut c = at.strip_prefix(&installs).ok()?.iter();
    let (key, v) = (installs.join(c.next()?), c.next()?.to_str()?.to_owned());
    Some((key, v, c.as_path().to_owned()))
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
        let home = homed()?;
        let (dir, bin) = (root().join(tool), name(tool, m));
        let link = home.join(".local/bin").join(bin);
        let cur = current(&dir);
        let ran = from_mise().filter(|(_, v, _)| cur.as_ref().is_none_or(|c| newer(v, c)));
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
        let plist = home.join(format!("Library/LaunchAgents/{label}.plist"));
        let (gui, plist_s) = (format!("gui/{}", uid()), plist.display().to_string());
        let target = format!("{gui}/{label}");
        let said = out(Command::new("launchctl").args(["print", &target])).unwrap_or_default();
        let pid = home.join(format!(".local/state/hotswap/{tool}.reload"));
        let mark = fs::read_to_string(&pid).unwrap_or_default();
        // the last waiter failed every bootstrap: load the agent, unless it is loaded again (a
        // login, the mark stale: a logout fails every bootstrap) or the user disabled it, whose
        // choice stands; a failed load is one log line and the mark stays for the next try
        if mark.trim() == "unloaded" {
            let off = || {
                out(Command::new("launchctl").args(["print-disabled", &gui]))
                    .is_ok_and(|d| d.contains(&format!("\"{label}\" => disabled")))
            };
            let healed = !said.is_empty()
                || off()
                || out(Command::new("launchctl").args(["bootstrap", &gui, &plist_s])).map_or_else(
                    |w| {
                        let note = format!("hotswap: {target} not loaded from {plist_s}: {w}");
                        eprintln!("{note}");
                        fs::create_dir_all(home.join("Library/Logs")).ok();
                        (File::options().create(true).append(true))
                            .open(home.join(format!("Library/Logs/{label}.log")))
                            .and_then(|mut f| writeln!(f, "{note}"))
                            .ok();
                        false
                    },
                    |_| true,
                );
            if healed {
                fs::remove_file(&pid)?;
            }
        }
        // one waiter at a time: a setup step inside the same run finds the first one alive; a pid
        // reused after a logout killed the waiter is not one, since its command names no plist
        let waiting = out(Command::new("ps").args(["-ww", "-o", "command=", "-p", mark.trim()]))
            .is_ok_and(|c| c.contains(&plist_s));
        // loaded from this plist, so never a scratch HOME's; launchd re-reads it only on a bootstrap
        if !waiting
            && said.contains(&format!("path = {plist_s}"))
            && !said.contains(&format!("program = {}", link.display()))
        {
            fs::write(&plist, agent_plist(tool, bin))?;
            fs::create_dir_all(home.join("Library/Logs"))?;
            fs::create_dir_all(home.join(".local/state/hotswap"))?;
            let log = home.join(format!("Library/Logs/{label}.log"));
            let log = File::options().create(true).append(true).open(log)?;
            // its own group, writing only to the agent's log: it outlives the upgrade run it waits
            // out and holds no step's pipe open
            let waiter = (Command::new("/bin/sh").args(["-c", RELOAD, &target, &gui, &plist_s]))
                .arg(&pid)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?;
            fs::write(&pid, waiter.id().to_string())?;
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

/// The tool's lock beside its record in `state`, held until dropped; the kernel drops it if the
/// process dies.
fn lock(tool: &str, state: &Path) -> Result<File, String> {
    let lock = state.join(format!("{tool}.lock"));
    let f = fs::create_dir_all(state).and_then(|()| File::create(&lock));
    let f = f.map_err(|e| format!("{tool}: {}: {e}", lock.display()))?;
    f.try_lock()
        .map_err(|_| format!("{tool}: another hotswap is running"))?;
    Ok(f)
}

/// Drops what is the tool's alone, under its lock — record, latest hint, reload pid, versions, its
/// packslip dirs (trust state, export, stage) — so a reinstall starts clean; the pinned packslip
/// binary, shared, stays. `<tool> setup --remove` calls it.
///
/// # Errors
/// HOME unset, or another hotswap of the tool running.
pub fn forget(tool: &str) -> Result<(), String> {
    let state = homed()?.join(".local/state/hotswap");
    let _lock = lock(tool, &state)?;
    for e in ["json", "latest", "lock", "reload"] {
        fs::remove_file(state.join(format!("{tool}.{e}"))).ok();
    }
    for d in [root().join(tool), own(tool)] {
        fs::remove_dir_all(d).ok();
    }
    Ok(())
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
        let state = homed()?.join(".local/state/hotswap");
        let dir = root().join(tool);
        let (uid, path, hint) = (
            uid(),
            state.join(format!("{tool}.json")),
            state.join(format!("{tool}.latest")),
        );
        let _lock = (mode != "--check")
            .then(|| lock(tool, &state))
            .transpose()?;
        // a host the old release bumped through mise but never set up (a refused drain, a kill)
        // runs this through mise's shim with no layout, and a `current` whose version went is
        // repaired; a binary run from anywhere else (a checkout) touches nothing
        let dangling = dir.join("current").symlink_metadata().is_ok();
        if current(&dir).is_none() && mode != "--check" && (dangling || from_mise().is_some()) {
            ensure(tool, m)?;
        }
        let cur = current(&dir).ok_or(format!(
            "{tool}: no version under {}; run {bin} setup",
            dir.display()
        ))?;
        let old: Option<Record> = serde_json::from_slice(&fs::read(&path).unwrap_or_default()).ok();
        let (mut new, mut pending, me) =
            (cur.clone(), None, format!("dev.gigantic.{tool}.upgrade"));
        if mode != "--back" {
            // the releases/latest redirect names the tag and spends no API quota: the cheap
            // "anything newer?" before packslip, which chooses the version
            let url = format!("https://github.com/{}/releases/latest", m.repo);
            // `-q`: a ~/.curlrc never shapes it; followed to the end, so a renamed repo's 301 still
            // lands on the tag
            let to = out(cmd("curl")
                .args(["-q", "-fsSIL", "-o", "/dev/null", "-w", "%{url_effective}"])
                .arg(&url))
            .map_err(|w| format!("{tool}: {url}: {w}"))?;
            let latest = (to.rsplit_once("/tag/").map(|t| t.1.trim_start_matches('v')))
                .ok_or(format!("{tool}: {url} names no release"))?;
            // a release that went red waits for a newer one, unless retried
            let red = old.as_ref().and_then(|r| r.red.as_deref());
            let held = |v: &str| mode != "--retry" && red == Some(v);
            let wait = |v: &str| {
                println!("{tool}: {v} is red; waiting for a newer release");
                Ok(i32::from(mode.is_empty()))
            };
            if held(latest) {
                return wait(latest);
            }
            // what packslip chose for this redirect within the hour, honoured by the agent's own
            // tick (launchd names it in XPC_SERVICE_NAME) and reported by --check: while it is not
            // newer, or red, no install is spent on it — a withdrawn latest, or one whose signed
            // bundle is not up yet, which the hour's expiry then retries
            let fresh = (fs::metadata(&hint).and_then(|f| f.modified()).ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < Duration::from_secs(3600));
            let tick = mode == "--check"
                || (mode.is_empty() && env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == me));
            let known = (fs::read_to_string(&hint).ok())
                .filter(|_| fresh && tick)
                .and_then(|h| Some(h.strip_prefix(&format!("{latest} "))?.to_owned()))
                .filter(|k| !newer(k, &cur) || held(k));
            if let Some(k) = known.filter(|_| newer(latest, &cur)) {
                if held(&k) {
                    return wait(&k);
                }
                pending = Some(latest.to_owned());
            } else if mode == "--check" && newer(latest, &cur) {
                new = latest.into();
            } else if newer(latest, &cur) {
                let v = fetch(tool, m, &cur).map_err(|e| format!("{tool}: {e}"))?;
                persist(&hint, format!("{latest} {v}").as_bytes()).ok();
                if held(&v) {
                    return wait(&v);
                }
                if newer(&v, &cur) {
                    new = v;
                }
            }
        }
        let run = |argv: &[String]| {
            let mut c = cmd(argv.first().map_or("", String::as_str));
            c.args(argv.iter().skip(1));
            let (code, out, _) = run(c, m.timeout.unwrap_or(120));
            (code, out)
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
            let pending = pending.map_or(String::new(), |l| format!("; {l} pending verification"));
            println!("{tool} {}: {state}{pending}", r.version);
            return Ok(i32::from(!r.ok && mode != "--check"));
        }
        // Setup, then the kickstarts (a new agent is loaded only once setup ran), then the smoke
        // probes; never the tool's own upgrade agent, whose kick would kill this very swap.
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
            (serde_json::to_vec(&r).map_err(io::Error::from))
                .and_then(|b| persist(&path, &b))
                .map_err(|e| format!("{tool}: {}: {e}", path.display()))
        };
        let (prev, before, green) = old.map_or((None, None, false), |r| {
            (Some(r.version), r.previous, r.ok || r.held)
        });
        let mut early = None;
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
            // previous version's green; a failed save still rolls back, and is reported after
            early = save(&mut rec).err();
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
        let late = save(&mut rec).err();
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
        match [early, late].into_iter().flatten().collect::<Vec<_>>() {
            e if e.is_empty() => Ok(i32::from(!rec.ok)),
            e => Err(e.join("; ")),
        }
    })()
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        2
    })
}
