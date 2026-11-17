//! The suite is also the tool: run under a tool's name (argv[0]'s file name) it is that tool's
//! `upgrade` — or, given `setup`, its `hotswap::ensure` — linking hotswap with the manifest named
//! for it below, or in `MANIFEST`; run bare by cargo, it drives itself through every branch, and
//! with `--ignored` (`cargo test -- --ignored`) through the real packslip and real releases.
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, File},
    os::unix::{fs::PermissionsExt, fs::symlink, process::CommandExt},
    path::Path,
    process::{Command, exit},
    thread,
    time::{Duration, Instant},
};

fn main() {
    let mut args = env::args();
    let argv0 = args.next().unwrap_or_default();
    // the file name, so a shim's exec of `…/installs/<key>/<v>/m` is tool `m`
    let tool =
        (Path::new(&argv0).file_name()).map_or(String::new(), |n| n.to_string_lossy().into());
    let m = json!({
        "bare": {"repo": "x/bare"},
        "gone": {"bin": "g", "repo": "x/g", "smoke": [["absent"]]},
        "m": {"repo": "x/m", "setup": ["m", "setup"], "signer": "ps1_m", "smoke": [["chk", "m"]]},
        "slow": {"fetch_timeout": 1, "repo": "x/slow", "smoke": [["sh", "-c", "sleep 3 & echo up"], ["sh", "-c", "(sleep 2; touch ~/m) & sleep 3"]], "timeout": 1},
        "t": {"agents": ["dev.gigantic.t.upgrade", "dev.gigantic.t"], "drain": [["t", "drain"]], "repo": "x/t", "setup": ["t", "setup"], "signer": "ps1_t", "smoke": [["t", "version"], ["chk", "x"]]},
    });
    let given = env::var("MANIFEST")
        .ok()
        .map(|j| serde_json::from_str(&j).unwrap());
    let of = |t: &str| {
        serde_json::from_value(given.clone().or(m.get(t).cloned()).unwrap_or(json!({}))).unwrap()
    };
    // `<tool> upgrade …` as an agent runs it, the verb dropped as a tool's main drops it
    let args = args.skip_while(|a| a == "upgrade").collect::<Vec<_>>();
    let known = given.is_some() || m.get(&tool).is_some();
    match tool.as_str() {
        "forget" => {
            if let Err(e) = hotswap::forget("t") {
                eprintln!("{e}");
                exit(2)
            }
        }
        "install" => exit(hotswap::install("t", &of("t"))),
        "plist" => print!("{}", hotswap::agent_plist("t", "tb")),
        t if known && args == ["setup"] => match hotswap::ensure(t, &of(t)) {
            Ok(p) => println!("{}", p.display()),
            Err(e) => {
                eprintln!("{e}");
                exit(2)
            }
        },
        t if known => exit(hotswap::upgrade(t, &of(t), &args)),
        _ if args.iter().any(|a| a == "--ignored") => real(),
        _ => suite(),
    }
}

/// The real thing, in a scratch HOME with launchctl stubbed: bootstrap the pinned packslip from two
/// tools at once and install the latest grove and routines, upgrade onto it from an older real release (pruning the oldest),
/// roll back by `--back` and by a red smoke, and refuse a wrong signer. Network, so ignored.
fn real() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let root = p.join(".local/share/hotswap");
    let lctl = p.join("bin/launchctl");
    fs::create_dir_all(lctl.parent().unwrap()).unwrap();
    fs::write(&lctl, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&lctl, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", p.join("bin").display());
    let signer = "ps1_rahbrmrvyrwx2q75yvkfebgfzy";
    let tool = |name: &str, manifest: Value, args: &[&str]| {
        let started = Instant::now();
        let o = (Command::new(env::current_exe().unwrap())
            .arg0(name)
            .args(args))
        .env("HOME", p)
        .env("PATH", &path)
        .env("MANIFEST", manifest.to_string())
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
        let out = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
        println!(
            "{name} {}: {:.1?}, exit {:?}\n  {}",
            args.join(" "),
            started.elapsed(),
            o.status.code(),
            out.trim().replace('\n', "\n  ")
        );
        (o.status.code().unwrap_or(-1), out)
    };
    let grove = |smoke: &str, signer: &str, args: &[&str]| {
        let m =
            json!({"repo": "jgeschwendt/grove", "signer": signer, "smoke": [["sh", "-c", smoke]]});
        tool("grove", m, args)
    };
    let version = || {
        let o = Command::new(p.join(".local/bin/grove"))
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    let current = || {
        fs::read_link(root.join("grove/current"))
            .unwrap()
            .display()
            .to_string()
    };
    let versions = || {
        let mut v = (fs::read_dir(root.join("grove")).unwrap().flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        v.sort();
        v.join(" ")
    };
    let state =
        || fs::read_to_string(p.join(".local/state/hotswap/grove.json")).unwrap_or_default();
    // each tool installed alone, then both at once: packslip bootstrapped by its pinned sha256
    // under two temp names, the latest of each relocated into the layout and run from there, each
    // tool's packslip dirs its own, so neither waits on the other; packslip's exports stay off
    // ~/.local/bin
    let routines = json!({"bin": "routine", "repo": "jgeschwendt/routines", "signer": "ps1_t5o72jatmrlk56zmnxvdyjrvry"});
    let unmade = || {
        for d in [".local/share/hotswap", ".local/bin", ".local/state"] {
            fs::remove_dir_all(p.join(d)).ok();
        }
    };
    let timed = |f: &dyn Fn() -> i32| {
        let started = Instant::now();
        assert_eq!(f(), 0);
        started.elapsed()
    };
    let solo_routines = timed(&|| tool("routines", routines.clone(), &["setup"]).0);
    unmade();
    let solo_grove = timed(&|| grove("true", signer, &["setup"]).0);
    unmade();
    let both = timed(&|| {
        thread::scope(|s| {
            let r = s.spawn(|| tool("routines", routines.clone(), &["setup"]).0);
            grove("true", signer, &["setup"]).0 + r.join().unwrap()
        })
    });
    println!(
        "solo routines {solo_routines:.1?}, solo grove {solo_grove:.1?}, both at once {both:.1?}"
    );
    let latest = current();
    assert_eq!(version(), format!("grove {latest}"));
    let local = fs::read_dir(p.join(".local/bin"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect::<Vec<_>>();
    let mut local = local;
    local.sort();
    assert_eq!(local, ["grove", "routine"]);
    let routine = Command::new(p.join(".local/bin/routine"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(routine.status.success());
    // two older releases beside it, the newest gone, a green record on the middle one
    let ps = root.join("packslip/1.11.4/packslip");
    for v in ["2026.11.1", "2026.11.2"] {
        let staged = root.join("stage/grove");
        let o = (Command::new(&ps).args([
            "install",
            "github.com/jgeschwendt/grove",
            "--version",
            v,
            "--pin",
            signer,
        ]))
        .arg("--install-dir")
        .arg(&staged)
        .arg("--bin-dir")
        .arg(root.join("packslip/bin"))
        .env("HOME", p)
        .env("XDG_CONFIG_HOME", root.join("packslip/config"))
        .env("XDG_DATA_HOME", root.join("packslip/data"))
        .env("XDG_STATE_HOME", root.join("packslip/state"))
        .output()
        .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        fs::rename(&staged, root.join("grove").join(v)).unwrap();
    }
    fs::remove_dir_all(root.join("grove").join(&latest)).unwrap();
    fs::remove_file(root.join("grove/current")).unwrap();
    symlink("2026.11.2", root.join("grove/current")).unwrap();
    let r = json!({"at": 0, "ok": true, "smoke": [], "version": "2026.11.2"});
    fs::create_dir_all(p.join(".local/state/hotswap")).unwrap();
    fs::write(p.join(".local/state/hotswap/grove.json"), r.to_string()).unwrap();
    assert_eq!(version(), "grove 2026.11.2");
    // upgrade: latest fetched and verified, green; the oldest pruned
    let smoke = "grove --version";
    assert_eq!(grove(smoke, signer, &[]).0, 0);
    assert_eq!(
        (current(), version()),
        (latest.clone(), format!("grove {latest}"))
    );
    assert_eq!(versions(), format!("2026.11.2 {latest} current"));
    // a deliberate rollback, no network; then the left version is held
    assert_eq!(grove(smoke, signer, &["--back"]).0, 0);
    assert_eq!(version(), "grove 2026.11.2");
    assert_eq!(grove(smoke, signer, &[]).0, 1);
    // --retry onto latest, whose smoke goes red: rolled back to 2026.11.2, which holds
    let only = "grove --version | grep -qx 'grove 2026.11.2'";
    let (code, out) = grove(only, signer, &["--retry"]);
    assert_eq!(
        (code, out.trim()),
        (
            1,
            &*format!("grove: sh -c {only} failed; rolled back to 2026.11.2")
        )
    );
    assert_eq!(version(), "grove 2026.11.2");
    // a wrong signer is refused by packslip: nothing moves, nothing recorded
    let before = state();
    let (code, out) = grove(smoke, "ps1_aaaaaaaaaaaaaaaaaaaaaaaaaa", &["--retry"]);
    assert_eq!(
        (code, out.trim()),
        (
            2,
            "grove: packslip install failed: signer does not match constraint 0's signer fingerprints"
        )
    );
    assert_eq!((current(), state()), ("2026.11.2".into(), before));
    println!("real: packslip 1.11.4, grove 2026.11.1 · 2026.11.2 → {latest} and back");
}

/// A run under a scratch HOME: exit code, output, then the stubs' log.
type Launch<'a> = dyn Fn(&mut Command, &[&str], &[(&str, &str)]) -> String + 'a;

/// `packslip.<pid>.tmp` read as `packslip.$PID.tmp`: a bootstrap's temp name is its own.
fn unpid(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("packslip.") {
        let (head, tail) = rest.split_at(i + "packslip.".len());
        out.push_str(head);
        let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && tail[digits..].starts_with(".tmp") {
            out.push_str("$PID");
        } else {
            out.push_str(&tail[..digits]);
        }
        rest = &tail[digits..];
    }
    out + rest
}

// One host walked through every branch. `curl`, `launchctl`, `uname` and `chk` are one sh stub
// that logs its argv and prints `<name>.<verb>.out`, else `<name>.out`, else `<verb> out`;
// `<name>.code`, `<name>.<verb>.code` or `<name>@<active version>.code` makes it exit non-zero, and
// `quiet` silences its stderr. A call from the reload waiter logs to `waiter.log`. launchctl's
// `print` fails between a `bootout` and the next good `bootstrap`, and a one-shot `once` fails a
// bootstrap. curl models GitHub's redirect: `releases/latest` lands on `…/releases/tag/v<tag>`, or
// on `…/releases` with no `tag`; a repo renamed per `renamed` (`old new`) first answers a 301 to
// the new name's `releases/latest`, which only `-L` follows; its download writes the
// packslip stub, or `garbage` when that exists; `shasum` and
// `sha256sum` answer `sum` for a file carrying the stub's marker. The packslip stub unpacks the tool
// stub as version `next`, with `bin/<bin>` when the bin is not `t`, or prints nothing
// (`packslip.silent`), exports nothing (`packslip.nobin`), dies by SIGKILL (`packslip.kill`) or
// hangs with a child that would touch `hung` (`packslip.hang`);
// the tool stub logs `<bin>@<its version>`, so a log shows which version each step ran, and a
// one-shot `crash.<version>` makes its `setup` SIGKILL its caller. `old/t` leads PATH, as a shim
// leaves the running install's bin dir: a step that runs it fails the exact log match. The temp
// dir's `&<>` must be escaped in the plist.
fn suite() {
    let d = tempfile::Builder::new().prefix("hot&<>").tempdir().unwrap();
    let p = d.path();
    let (bin, old, root) = (p.join("bin"), p.join("old"), p.join(".local/share/hotswap"));
    let ts = root.join("t");
    let stub = format!(
        "#!/bin/sh\nn=${{0##*/}}; v=$(readlink '{t}/current'); log=log\ncase \"$(ps -ww -o command= -p $PPID)\" in *bootout*) log=waiter.log;; esac\necho \"$n $*\" >> '{p}'/$log\nif [ \"$n\" = curl ] && [ ! -e '{p}/curl.code' ]; then case \"$*\" in *releases/latest) for u; do :; done; r=${{u#https://github.com/}}; r=${{r%/releases/latest}}; [ \"$(cut -d' ' -f1 '{p}/renamed' 2>/dev/null)\" = \"$r\" ] && {{ r=$(cut -d' ' -f2 '{p}/renamed'); case \"$*\" in *-fsSIL*) ;; *) printf 'https://github.com/%s/releases/latest' \"$r\"; exit 0;; esac; }}; t=$(cat '{p}/tag' 2>/dev/null); if [ -n \"$t\" ]; then printf 'https://github.com/%s/releases/tag/v%s' \"$r\" \"$t\"; else printf 'https://github.com/%s/releases' \"$r\"; fi; exit 0;; esac; fi\n[ \"$n $2\" = 'curl -fsSL' ] && {{ cat '{p}/garbage' 2>/dev/null || cat '{p}/packslip.stub'; }} > \"$4\"\n[ \"$n $1\" = 'launchctl bootout' ] && touch '{p}/booted'\n[ \"$n $1\" = 'launchctl print' ] && [ -e '{p}/booted' ] && exit 113\n[ \"$n $1\" = 'launchctl bootstrap' ] && rm '{p}/once' 2>/dev/null && echo 'Bootstrap failed: 5: Input/output error' >&2 && exit 5\n[ \"$n $1\" = 'launchctl bootstrap' ] && [ ! -e '{p}/launchctl.bootstrap.code' ] && rm -f '{p}/booted'\ncat '{p}'/\"$n.$1.out\" 2>/dev/null || cat '{p}'/\"$n.out\" 2>/dev/null || echo \"$1 out\"; [ -e '{p}/quiet' ] || echo \"$1 err\" >&2\nexit $(cat \"{p}/$n.code\" 2>/dev/null || cat \"{p}/$n.$1.code\" 2>/dev/null || cat \"{p}/$n@$v.code\" 2>/dev/null || cat \"{p}/$n.$1@$(readlink '{m}').code\" 2>/dev/null || echo 0)\n",
        t = ts.display(),
        m = root.join("m/current").display(),
        p = p.display()
    );
    let hash = format!(
        "#!/bin/sh\nfor f; do :; done\necho \"${{0##*/}} $*\" >> '{0}/log'\n[ -e \"$f\" ] || {{ echo \"$f: No such file or directory\" >&2; exit 1; }}\ngrep -q packslip-stub \"$f\" && echo \"$(cat '{0}/sum')  $f\" || echo \"bad  $f\"\n",
        p.display()
    );
    let tool = format!(
        "#!/bin/sh\nn=${{0##*/}}; r=$(readlink -f \"$0\"); r=${{r#*/hotswap/*/}}; v=${{r%%/*}}\necho \"$n@$v $*\" >> '{0}/log'\n[ \"$1\" = setup ] && rm \"{0}/crash.$v\" 2>/dev/null && kill -9 $PPID\necho \"$1 out\"; [ -e '{0}/quiet' ] || echo \"$1 err\" >&2\nexit $(cat \"{0}/$n.code\" 2>/dev/null || cat \"{0}/$n.$1.code\" 2>/dev/null || echo 0)\n",
        p.display()
    );
    let packslip = format!(
        "#!/bin/sh\n# packslip-stub\necho \"packslip $* state=$XDG_STATE_HOME\" >> '{0}/log'\n[ -e '{0}/packslip.code' ] && printf 'Error: \\n   0: \\033[91mno release\\033[0m\\n\\nLocation:\\n' >&2 && exit 1\n[ -e '{0}/packslip.kill' ] && kill -9 $$\n[ -e '{0}/packslip.hang' ] && {{ (sleep 2; touch '{0}/hung') & sleep 30; }}\n[ -e '{0}/packslip.silent' ] && exit 0\nn=${{2##*/}}; rel=$n; [ \"$n\" = t ] || [ \"$n\" = m ] || rel=bin/$n\nmkdir -p \"$(dirname \"$6/$rel\")\" && cp '{0}/tool.stub' \"$6/$rel\"\nif [ -e '{0}/packslip.nobin' ]; then rm -f \"$8/$n\"; else ln -sf \"$(cd \"$6\" && pwd -P)/$rel\" \"$8/$n\"; fi\necho \"Installed $2 $(cat '{0}/next') at $6\"\n",
        p.display()
    );
    let exe = |path: &Path, body: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    for name in ["chk", "curl", "launchctl", "uname"] {
        exe(&bin.join(name), &stub);
    }
    for name in ["sha256sum", "shasum"] {
        exe(&bin.join(name), &hash);
    }
    exe(
        &old.join("t"),
        &format!("#!/bin/sh\necho \"old t $*\" >> '{}/log'\n", p.display()),
    );
    exe(&p.join("packslip.stub"), &packslip);
    exe(&p.join("tool.stub"), &tool);
    let put_file = |name: &str, body: &str| fs::write(p.join(name), body).unwrap();
    let code = |name: &str| put_file(name, "3");
    let rm = |name: &str| fs::remove_file(p.join(name)).unwrap();
    let tag = |v: &str| put_file("tag", v);
    put_file("uname.out", "Darwin arm64\n");
    let sum = "2b21dc68d54c23a56123f9f9d7402a8b9cfb1e6950c5d4966730ec5cee20146b";
    put_file("sum", sum);
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let norm = |s: &str| {
        unpid(s)
            .replace(&format!("gui/{}", uid.trim()), "gui/$UID")
            .replace(&p.display().to_string(), "~")
    };
    // exit code, stdout + stderr, then the stubs' log, the temp dir read as `~`
    let path = format!("{}:{}:/bin:/usr/bin", old.display(), bin.display());
    let launch = |c: &mut Command, args: &[&str], envs: &[(&str, &str)]| {
        let o = (c.args(args).env("HOME", p).env("PATH", &path))
            .env_remove("MISE_DATA_DIR")
            .env_remove("XDG_DATA_HOME")
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        let log = fs::read_to_string(p.join("log")).unwrap_or_default();
        fs::remove_file(p.join("log")).ok();
        let out = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
        // a signal death reads -1
        norm(&format!("{}\n{out}{log}", o.status.code().unwrap_or(-1)))
    };
    let go = |tool: &str, args: &[&str], envs: &[(&str, &str)]| {
        launch(
            Command::new(env::current_exe().unwrap()).arg0(tool),
            args,
            envs,
        )
    };
    let run = |args: &[&str]| go("t", args, &[]);
    // the agent's own tick: launchd names its job in XPC_SERVICE_NAME
    let tick = |t: &str| {
        let label = format!("dev.gigantic.{t}.upgrade");
        go(t, &[], &[("XPC_SERVICE_NAME", &label)])
    };
    let state_dir = p.join(".local/state/hotswap");
    let rec = |tool: &str| {
        serde_json::from_slice::<Value>(&fs::read(state_dir.join(format!("{tool}.json"))).unwrap())
            .unwrap()
    };
    let state = || {
        let r = rec("t");
        let f = |k: &str| r[k].to_string();
        [f("version"), f("previous"), f("ok"), f("failed"), f("red")].join(" ")
    };
    let probes = || {
        let r = rec("t");
        let s = r["smoke"].as_array().unwrap().iter();
        s.map(|s| format!("{} {}", s["probe"], s["code"]))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // a green record at `v`, whose previous is 0 — a version the layout never holds
    let put = |v: &str| {
        let r = json!({"at": 0, "ok": true, "previous": "0", "smoke": [], "version": v});
        fs::write(state_dir.join("t.json"), r.to_string()).unwrap();
    };
    // the versions under the root, the active one marked
    let versions = |dir: &Path| {
        let cur = fs::read_link(dir.join("current")).unwrap();
        let mut v = (fs::read_dir(dir).unwrap().flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "current")
            .map(|n| {
                if cur == Path::new(&n) {
                    format!("{n}*")
                } else {
                    n
                }
            })
            .collect::<Vec<_>>();
        v.sort();
        v.join(" ")
    };
    let lay = |v: &str| exe(&ts.join(v).join("t"), &tool);
    let repoint = |v: &str| {
        fs::remove_file(ts.join("current")).unwrap();
        symlink(v, ts.join("current")).unwrap();
    };
    let latest = |repo: &str| {
        format!(
            "curl -q -fsSIL -o /dev/null -w %{{url_effective}} https://github.com/{repo}/releases/latest\n"
        )
    };
    let t_latest = latest("x/t");
    let pk = |tool: &str, repo: &str, pin: &str| {
        format!(
            "packslip install github.com/{repo} --pin {pin} --install-dir ~/.local/share/hotswap/packslip/{tool}/stage --bin-dir ~/.local/share/hotswap/packslip/{tool}/bin state=~/.local/share/hotswap/packslip/{tool}/state\n"
        )
    };
    let uname = "uname -sm\n";
    let checked = "shasum -a 256 ~/.local/share/hotswap/packslip/1.11.4/packslip\n";
    let fetch =
        |tool: &str, repo: &str, pin: &str| format!("{uname}{checked}{}", pk(tool, repo, pin));
    let t_fetch = fetch("t", "x/t", "ps1_t");
    let t_pk = pk("t", "x/t", "ps1_t");
    let print = |t: &str| format!("launchctl print gui/$UID/dev.gigantic.{t}.upgrade\n");
    let t_print = print("t");
    let steps = |v: &str| {
        format!(
            "t@{v} setup\nlaunchctl kickstart -k gui/$UID/dev.gigantic.t\nt@{v} version\nchk x\n"
        )
    };
    // the drain runs on the active version, before `current` moves to the new one
    let swap = |from: &str, to: &str| format!("t@{from} drain\n{}", steps(to));
    let red = |why: &str, log: &str| format!("1\nt: chk x failed; {why}\n{t_latest}{log}");

    let usage = "2\nusage: t upgrade [--check | --back | --retry]\n";
    assert_eq!(run(&["--x"]), usage);
    assert_eq!(run(&["--back", "--check"]), usage);
    assert_eq!(go("gone", &["--x"], &[]), usage.replace(" t ", " g "));
    // HOME unset is refused everywhere: a relative path would write into the cwd
    let no_home = [("HOME", "")];
    assert_eq!(go("t", &[], &no_home), "2\nHOME unset\n");
    assert_eq!(go("t", &["setup"], &no_home), "2\nt: HOME unset\n");
    assert_eq!(go("install", &[], &no_home), "2\nt: HOME unset\n");
    assert_eq!(go("forget", &[], &no_home), "2\nHOME unset\n");
    // a binary run from a checkout, with no layout and not from mise, touches nothing
    let none = "2\nt: no version under ~/.local/share/hotswap/t; run t setup\n";
    assert_eq!(run(&["--check"]), none);
    assert_eq!(run(&[]), none);
    assert!(!root.exists() && !p.join(".local/bin").exists());

    // the plist runs the link on a PATH without mise
    let home = p
        .display()
        .to_string()
        .replace("hot&<>", "hot&amp;&lt;&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.gigantic.t.upgrade</string>
  <key>ProgramArguments</key>
  <array>
    <string>{home}/.local/bin/tb</string>
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
  <key>StandardOutPath</key><string>{home}/Library/Logs/dev.gigantic.t.upgrade.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/dev.gigantic.t.upgrade.log</string>
</dict>
</plist>
"#
    );
    assert_eq!(
        go("plist", &[], &[]),
        format!("0\n{plist}").replace(&*p.display().to_string(), "~")
    );

    // the first install: packslip refused on an unsupported host, its download failing, then
    // failing its pinned sha256 (on Linux too, through sha256sum), then bootstrapped; its install
    // failing, dying, naming no version, exporting no bin; garbage where packslip was is fetched
    // again; a stage left unremovable is made removable, or named; then through
    let ps = root.join("packslip/1.11.4/packslip");
    let tmp = "~/.local/share/hotswap/packslip/1.11.4/packslip.$PID.tmp";
    let url = "https://packslip.dev/v1.11.4/packslip-v1.11.4-darwin-arm64";
    let dl = format!("curl -q -fsSL -o {tmp} {url}\n");
    let boot = format!("{uname}{checked}{dl}shasum -a 256 {tmp}\n");
    let temps = || {
        (fs::read_dir(ps.parent().unwrap())
            .into_iter()
            .flatten()
            .flatten())
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
    };
    put_file("uname.out", "Darwin x86_64\n");
    assert_eq!(
        go("install", &[], &[]),
        format!(
            "2\nt: Darwin x86_64 is unsupported: packslip builds darwin-arm64, linux-arm64 and linux-x64\n{uname}"
        )
    );
    put_file("uname.out", "Darwin arm64\n");
    code("curl.code");
    let err = "2\nt: packslip 1.11.4 bootstrap: ";
    assert_eq!(
        go("install", &[], &[]),
        format!("{err}-q err\n{uname}{checked}{dl}")
    );
    assert!(!temps());
    rm("curl.code");
    put_file("garbage", "#!/bin/sh\n");
    let mismatch = |url: &str, sum: &str| format!("{url} is not the pinned sha256 {sum}");
    assert_eq!(
        go("install", &[], &[]),
        format!("{err}{}\n{boot}", mismatch(url, sum))
    );
    assert!(!temps() && !ps.exists());
    rm("garbage");
    code("packslip.code");
    assert_eq!(
        go("install", &[], &[]),
        format!("2\nt: packslip install failed: no release\n{boot}{t_pk}")
    );
    assert_eq!(
        fs::metadata(&ps).unwrap().permissions().mode() & 0o777,
        0o755
    );
    rm("packslip.code");
    let linux = "bc6211617a25f0ea8c0638cc2295aa9251b7f54c5087105bb46bf44f6357e8c1";
    put_file("uname.out", "Linux x86_64\n");
    let lx = "https://packslip.dev/v1.11.4/packslip-v1.11.4-linux-x64";
    assert_eq!(
        go("install", &[], &[]),
        format!(
            "{err}{}\n{uname}{}curl -q -fsSL -o {tmp} {lx}\nsha256sum {tmp}\n",
            mismatch(lx, linux),
            checked.replace("shasum -a 256", "sha256sum")
        )
    );
    put_file("uname.out", "Darwin arm64\n");
    fs::set_permissions(&ps, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        go("install", &[], &[]),
        format!("2\nt: packslip install failed: Permission denied (os error 13)\n{uname}{checked}")
    );
    fs::set_permissions(&ps, fs::Permissions::from_mode(0o755)).unwrap();
    for (flag, why) in [
        (
            "packslip.kill",
            "packslip install failed: killed by signal 9",
        ),
        ("packslip.silent", "packslip named no version"),
        ("packslip.nobin", "x/t 2 exports no t"),
    ] {
        put_file("next", "2");
        put_file(flag, "");
        assert_eq!(go("install", &[], &[]), format!("2\nt: {why}\n{t_fetch}"));
        rm(flag);
    }
    // a stage holding a dir its owner cannot list goes; one whose parent is read-only is named
    let (own, staged) = (root.join("packslip/t"), root.join("packslip/t/stage"));
    fs::create_dir_all(staged.join("x")).unwrap();
    fs::set_permissions(&own, fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(
        go("install", &[], &[]),
        format!(
            "2\nt: ~/.local/share/hotswap/packslip/t/stage: Permission denied (os error 13)\n{uname}{checked}"
        )
    );
    fs::set_permissions(&own, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(staged.join("x")).unwrap();
    fs::set_permissions(staged.join("x"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(&ps, "garbage").unwrap();
    assert_eq!(
        go("install", &[], &[]),
        format!("0\nsetup out\nsetup err\n{boot}{t_pk}{t_print}t@2 setup\n")
    );
    assert!(!temps());
    assert_eq!(versions(&ts), "2*");
    assert_eq!(
        fs::read_link(p.join(".local/bin/t")).unwrap(),
        ts.join("current/t")
    );
    assert!(!staged.exists());
    // a second install fetches nothing: setup alone, its failure exit 1; XDG_DATA_HOME moves
    // nothing, the root is HOME's
    code("t.setup.code");
    assert_eq!(
        go("install", &[], &[("XDG_DATA_HOME", "/x")]),
        format!("1\nsetup out\nsetup err\n{t_print}t@2 setup\n")
    );
    rm("t.setup.code");
    assert_eq!(run(&["setup"]), format!("0\n~/.local/bin/t\n{t_print}"));
    // a `current` whose version went is repaired by the next upgrade even off mise; the repair's
    // failed install leaves no stage
    fs::rename(ts.join("2"), p.join("t2")).unwrap();
    code("packslip.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: packslip install failed: no release\n{t_fetch}")
    );
    assert!(!staged.exists());
    rm("packslip.code");
    assert_eq!(
        run(&[]),
        format!(
            "2\nt: https://github.com/x/t/releases/latest names no release\n{t_fetch}{t_print}{t_latest}"
        )
    );
    assert_eq!(versions(&ts), "2*");
    fs::remove_dir_all(p.join("t2")).unwrap();

    // the latest check failing, then naming no release: nothing tried; a renamed repo's redirect
    // lands on its tag all the same
    code("curl.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: https://github.com/x/t/releases/latest: -q err\n{t_latest}")
    );
    rm("curl.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: https://github.com/x/t/releases/latest names no release\n{t_latest}")
    );
    // the old repo's URL redirects to the renamed one's tag
    put_file("renamed", "x/t x/t-renamed");
    tag("2");
    assert_eq!(run(&["--back"]), "2\nt: no previous version\n");
    let plan = swap("2", "2")
        .trim_end()
        .replace('\n', "\n  ")
        .replace("t@2 ", "t ");
    assert_eq!(
        run(&["--check"]),
        format!("0\nt: none → 2\n  {plan}\n{t_latest}")
    );

    // red with no previous version: stays put, held while it is latest
    code("chk.code");
    assert_eq!(run(&[]), red("no previous version", &swap("2", "2")));
    assert_eq!(state(), r#""2" null false "chk x" "2""#);
    let wait = |v: &str| format!("t: {v} is red; waiting for a newer release");
    assert_eq!(run(&[]), format!("1\n{}\n{t_latest}", wait("2")));
    assert_eq!(run(&["--check"]), format!("0\n{}\n{t_latest}", wait("2")));
    rm("chk.code");

    // a newer release: packslip's install failing changes nothing; a refused drain leaves the
    // active version active, the fetched one beside it
    tag("3");
    put_file("next", "3");
    code("packslip.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: packslip install failed: no release\n{t_latest}{t_fetch}")
    );
    assert_eq!(state(), r#""2" null false "chk x" "2""#);
    rm("packslip.code");
    code("t.drain.code");
    assert_eq!(
        run(&[]),
        format!("1\nt: t drain refused; nothing swapped\n{t_latest}{t_fetch}t@2 drain\n")
    );
    assert_eq!(versions(&ts), "2* 3");
    rm("t.drain.code");

    // green on 3: the fetch is a no-op once 3 is on disk, and the green prunes past previous
    put("2");
    lay("1");
    assert_eq!(
        run(&[]),
        format!(
            "0\nt 3: green, 2 smoke probes\n{t_latest}{t_fetch}{}",
            swap("2", "3")
        )
    );
    assert_eq!(state(), r#""3" "2" true null null"#);
    assert_eq!(versions(&ts), "2 3*");
    let smoke = json!([{"code": 0, "output": "version out\nversion err\n", "probe": "t version"}, {"code": 0, "output": "x out\nx err\n", "probe": "chk x"}]);
    assert_eq!(rec("t")["smoke"], smoke);
    assert_eq!(run(&[]), format!("0\nt 3: up to date\n{t_latest}"));
    assert_eq!(run(&["--check"]), format!("0\nt 3: up to date\n{t_latest}"));
    // a newer release in --check is reported, not fetched
    tag("4");
    let plan = swap("3", "3")
        .trim_end()
        .replace('\n', "\n  ")
        .replace("t@3 ", "t ");
    assert_eq!(
        run(&["--check"]),
        format!("0\nt: 3 → 4\n  {plan}\n{t_latest}")
    );
    // packslip skips a withdrawn 4 and chooses 3: nothing newer, nothing kept, and within the
    // hour the agent's tick spends no install on it, as --check says; a run by hand does
    assert_eq!(run(&[]), format!("0\nt 3: up to date\n{t_latest}{t_fetch}"));
    assert_eq!(versions(&ts), "2 3*");
    assert!(!staged.exists());
    let pending = format!("0\nt 3: up to date; 4 pending verification\n{t_latest}");
    assert_eq!(tick("t"), pending);
    assert_eq!(run(&["--check"]), pending);
    assert_eq!(
        run(&["--back"]),
        format!("0\nt 2: green, 2 smoke probes\n{}", steps("2"))
    );
    assert_eq!(state(), r#""2" "3" true null "3""#);

    // a red swap whose record cannot be saved still rolls back, then names the failed saves
    tag("2");
    lay("1");
    put("1");
    code("chk@2.code");
    let rollback = steps("1");
    fs::create_dir(state_dir.join("t.json.tmp")).unwrap();
    let unsaved = "t: ~/.local/state/hotswap/t.json: Is a directory (os error 21)";
    assert_eq!(
        run(&[]),
        format!(
            "2\nt: chk x failed; rolled back to 1\n{unsaved}; {unsaved}\n{t_latest}{}{rollback}",
            swap("2", "2")
        )
    );
    assert_eq!(versions(&ts), "1* 2 3");
    fs::remove_dir(state_dir.join("t.json.tmp")).unwrap();
    repoint("2");
    // red with a previous version: roll back with no network, keeping the record's previous
    assert_eq!(
        run(&[]),
        red("rolled back to 1", &format!("{}{rollback}", swap("2", "2")))
    );
    assert_eq!(state(), r#""1" "0" false "chk x" "2""#);
    let back = r#""t version" 0, "chk x" 3, "t version" 0"#;
    assert_eq!(probes(), format!(r#"{back}, "chk x" 0"#));
    assert_eq!(versions(&ts), "1* 2 3");
    // the version rolled back from is held while it is latest; --retry takes it again, and a
    // version that held through a rollback is a rollback target again
    assert_eq!(run(&[]), format!("1\n{}\n{t_latest}", wait("2")));
    assert_eq!(run(&["--check"]), format!("0\n{}\n{t_latest}", wait("2")));
    put_file("next", "2");
    assert_eq!(
        run(&["--retry"]),
        red(
            "rolled back to 1",
            &format!("{t_fetch}{}{rollback}", swap("1", "2"))
        )
    );
    // a withdrawn latest whose stand-in went red is held too, after one install to learn it
    tag("9");
    assert_eq!(tick("t"), format!("1\n{}\n{t_latest}{t_fetch}", wait("2")));
    assert_eq!(tick("t"), format!("1\n{}\n{t_latest}", wait("2")));
    // a newer release goes through
    tag("3");
    put_file("next", "3");
    code("chk@3.code");
    assert_eq!(
        run(&[]),
        red(
            "rolled back to 1",
            &format!("{t_fetch}{}{rollback}", swap("1", "3"))
        )
    );
    // the rollback goes red too
    put("1");
    code("chk.code");
    assert_eq!(
        run(&[]),
        red(
            "rolled back to 1, where chk x failed too",
            &format!("{t_fetch}{}{rollback}", swap("1", "3"))
        )
    );
    assert_eq!(
        state(),
        r#""1" "0" false "chk x; after rollback chk x" "3""#
    );
    assert_eq!(probes(), format!(r#"{back}, "chk x" 3"#));
    assert_eq!(
        run(&["--retry"]),
        red(
            "1 is red; no rollback",
            &format!("{t_fetch}{}", swap("1", "3"))
        )
    );
    // `current` cannot move: the swap and the rollback are red at the repoint
    put("1");
    fs::create_dir_all(ts.join("current.tmp/x")).unwrap();
    rm("chk.code");
    let to = |v: &str| format!("current → {v}: File exists (os error 17)");
    assert_eq!(
        run(&[]),
        format!(
            "1\nt: {} failed; rolled back to 1, where {} failed too\n{t_latest}t@3 drain\n",
            to("3"),
            to("1")
        )
    );
    assert_eq!(
        state(),
        format!(
            r#""1" "0" false "{}; after rollback {}" "3""#,
            to("3"),
            to("1")
        )
    );
    fs::remove_dir_all(ts.join("current.tmp")).unwrap();
    // a crash in the rollback leaves the failed version's red record; once `current` moved, the
    // next run swaps the active version and never rolls back onto the red one
    put("2");
    put_file("crash.2", "");
    code("chk@3.code");
    assert_eq!(
        run(&[]),
        format!("-1\n{t_latest}{}t@2 setup\n", swap("3", "3"))
    );
    assert_eq!(state(), r#""3" "2" false "chk x" "3""#);
    tag("2");
    code("chk.code");
    assert_eq!(run(&[]), red("3 is red; no rollback", &swap("2", "2")));
    assert_eq!(state(), r#""2" "3" false "chk x" "2""#);
    // the previous version is gone from the layout
    put("0");
    assert_eq!(run(&[]), red("0 not installed", &swap("2", "2")));
    assert_eq!(state(), r#""2" "0" false "chk x" "2""#);
    assert_eq!(run(&["--back"]), "2\nt 0: not installed\n");
    // a deliberate rollback that goes red stays there
    let r = json!({"at": 0, "failed": "chk x", "ok": false, "previous": "1", "smoke": [], "version": "2"});
    fs::write(state_dir.join("t.json"), r.to_string()).unwrap();
    code("chk@1.code");
    assert_eq!(
        run(&["--back"]),
        format!(
            "1\nt: chk x failed on 1; no second rollback\n{}",
            steps("1")
        )
    );
    assert_eq!(state(), r#""1" "2" false "chk x" "2""#);
    for f in ["chk@1.code", "chk.code", "chk@2.code", "chk@3.code"] {
        rm(f);
    }

    // another upgrade holds the lock: refused, and so is forget; --check reads on,
    let held = File::create(state_dir.join("t.lock")).unwrap();
    held.try_lock().unwrap();
    assert_eq!(run(&[]), "2\nt: another hotswap is running\n");
    assert_eq!(go("forget", &[], &[]), "2\nt: another hotswap is running\n");
    // and a red release that is no longer latest stays red, settled
    tag("1");
    assert_eq!(
        run(&["--check"]),
        format!("0\nt 1: red at chk x\n{t_latest}")
    );
    drop(held);
    assert_eq!(run(&[]), format!("1\nt 1: red at chk x\n{t_latest}"));

    // a probe past its deadline is a red probe and its whole group dies; one whose background child
    // holds the pipe keeps its code
    let setup = |t: &str, repo: &str, bin: &str| {
        assert_eq!(
            go(t, &["setup"], &[]),
            format!("0\n~/.local/bin/{bin}\n{}{}", fetch(t, repo, ""), print(t))
        );
    };
    // a packslip that hangs is killed with its group at the deadline, leaving no stage; each tool
    // has its own packslip dirs, so none waits on another's lock
    put_file("next", "2");
    put_file("packslip.hang", "");
    assert_eq!(
        go("slow", &["setup"], &[]),
        format!(
            "2\nslow: packslip install failed: timed out after 1s\n{}",
            fetch("slow", "x/slow", "")
        )
    );
    assert!(!root.join("packslip/slow/stage").exists());
    rm("packslip.hang");
    setup("slow", "x/slow", "slow");
    let late = "sh -c (sleep 2; touch ~/m) & sleep 3";
    assert_eq!(
        go("slow", &[], &[]),
        format!(
            "1\nslow: {late} failed; no previous version\n{}",
            latest("x/slow")
        )
    );
    assert_eq!(
        rec("slow")["smoke"],
        json!([{"code": 0, "output": "output held open past 1s", "probe": "sh -c sleep 3 & echo up"}, {"code": -1, "output": "timed out after 1s", "probe": late}])
    );
    thread::sleep(Duration::from_secs(2));
    assert!(!p.join("m").exists() && !p.join("hung").exists());

    // a missing command is a red probe, and a release keeping its binary deeper gets a link at its
    // top; an unwritable lock or record exits 2
    setup("gone", "x/g", "g");
    assert_eq!(
        fs::read_link(root.join("gone/2/g")).unwrap(),
        Path::new("bin/g")
    );
    assert_eq!(
        go("gone", &[], &[]),
        format!(
            "1\ngone: absent failed; no previous version\n{}",
            latest("x/g")
        )
    );
    assert_eq!(rec("gone")["smoke"][0]["code"], -1);
    setup("bare", "x/bare", "bare");
    // the publish window: release 3 is public before its signed bundle, so packslip chooses 2,
    // the current one; within the hour the agent's tick trusts that and --check agrees, a run by
    // hand checks again, and once the hint is an hour old the tick installs 3, now verifiable
    let bare_hint = state_dir.join("bare.latest");
    tag("3");
    let bare_up =
        |more: &str, log: &str| format!("0\nbare 2: up to date{more}\n{}{log}", latest("x/bare"));
    assert_eq!(
        tick("bare"),
        format!(
            "0\nbare 2: green, 0 smoke probes\n{}{}",
            latest("x/bare"),
            fetch("bare", "x/bare", "")
        )
    );
    assert_eq!(fs::read_to_string(&bare_hint).unwrap(), "3 2");
    assert_eq!(tick("bare"), bare_up("; 3 pending verification", ""));
    assert_eq!(
        go("bare", &["--check"], &[]),
        bare_up("; 3 pending verification", "")
    );
    assert_eq!(
        go("bare", &[], &[]),
        bare_up("", &fetch("bare", "x/bare", ""))
    );
    let hour = Duration::from_secs(3601);
    let age = |h: &Path| {
        let f = File::options().write(true).open(h).unwrap();
        f.set_modified(std::time::SystemTime::now() - hour).unwrap();
    };
    age(&bare_hint);
    put_file("next", "3");
    assert_eq!(
        tick("bare"),
        format!(
            "0\nbare 3: green, 0 smoke probes\n{}{}",
            latest("x/bare"),
            fetch("bare", "x/bare", "")
        )
    );
    tag("1");
    let bare = "bare: ~/.local/state/hotswap/bare.";
    fs::remove_file(state_dir.join("bare.lock")).unwrap();
    fs::remove_file(state_dir.join("bare.json")).unwrap();
    for f in ["bare.lock", "bare.json.tmp"] {
        fs::create_dir(state_dir.join(f)).unwrap();
        let o = go("bare", &[], &[]);
        assert!(o.starts_with("2\n") && o.contains(bare), "{o}");
        fs::remove_dir(state_dir.join("bare.lock")).ok();
    }

    // forget drops what is the tool's — record, hint, lock, versions, stage, its packslip export —
    // and leaves the shared packslip, absent or not
    let export = root.join("packslip/t/bin/t");
    assert!(state_dir.join("t.latest").exists() && export.symlink_metadata().is_ok());
    for _ in 0..2 {
        assert_eq!(go("forget", &[], &[]), "0\n");
    }
    for f in ["t.json", "t.latest", "t.lock"] {
        assert!(!state_dir.join(f).exists());
    }
    assert!(!ts.exists() && !root.join("packslip/t").exists() && ps.exists());
    assert!(root.join("packslip/slow").is_dir());

    migration(p, &exe, &launch, &norm);
}

/// The move off mise, as the OLD release drives it: tool `m`, mise's versions 1 (an old release,
/// whose `setup` puts the link and the agent back on mise's shim) and 2 (this very binary) under
/// `$XDG_DATA_HOME/mise`; the shim execs mise's active version. An old tick bumps mise, then
/// drains, runs `m setup` through the shim, kickstarts and smokes, and on red `mise use`s 1 and
/// runs its setup.
fn migration(p: &Path, exe: &dyn Fn(&Path, &str), launch: &Launch, norm: &dyn Fn(&str) -> String) {
    let xdg = p.join("xdg");
    let installs = xdg.join("mise/installs/packslip-x-m");
    let (shim, link) = (xdg.join("mise/shims/m"), p.join(".local/bin/m"));
    let ms = p.join(".local/share/hotswap/m");
    let agent = p.join("Library/LaunchAgents/dev.gigantic.m.upgrade.plist");
    let agent_log = p.join("Library/Logs/dev.gigantic.m.upgrade.log");
    let put_file = |name: &str, body: &str| fs::write(p.join(name), body).unwrap();
    exe(
        &shim,
        &format!(
            "#!/bin/sh\nexec \"{}/$(cat '{}/active')/m\" \"$@\"\n",
            installs.display(),
            p.display()
        ),
    );
    let me = env::current_exe().unwrap();
    let copy_me = |v: &str| {
        fs::create_dir_all(installs.join(v)).unwrap();
        fs::copy(&me, installs.join(v).join("m")).unwrap();
    };
    copy_me("2");
    exe(
        &installs.join("1/m"),
        &format!(
            "#!/bin/sh\necho \"m@1 $*\" >> '{0}/log'\nln -sfn '{1}' '{2}'\necho \"mise's\" > '{3}'\n",
            p.display(),
            shim.display(),
            link.display(),
            agent.display()
        ),
    );
    fs::create_dir_all(installs.join(".mise")).unwrap();
    symlink("./2", installs.join("global")).unwrap();
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let loaded = |program: &Path, state: &str| {
        let out = format!(
            "path = {}\nstate = {state}\nprogram = {}\n",
            agent.display(),
            program.display()
        );
        put_file("launchctl.print.out", &out);
    };
    // the host as a mise install left it
    let on_mise = || {
        fs::remove_dir_all(&ms).ok();
        fs::remove_file(&link).ok();
        symlink(&shim, &link).unwrap();
        fs::write(&agent, "mise's\n").unwrap();
        // the old release's last green record, its shape unchanged
        let r = json!({"at": 0, "ok": true, "smoke": [], "version": "1"});
        fs::create_dir_all(p.join(".local/state/hotswap")).unwrap();
        fs::write(p.join(".local/state/hotswap/m.json"), r.to_string()).unwrap();
        put_file("active", "1");
        loaded(&shim, "not running");
    };
    let by_shim =
        |args: &[&str], envs: &[(&str, &str)]| launch(&mut Command::new(&shim), args, envs);
    let xdg_env = [("XDG_DATA_HOME", &*xdg.to_string_lossy().into_owned())];
    let xdg_env: Vec<(&str, &str)> = xdg_env.iter().map(|(k, v)| (*k, &**v)).collect();
    let mise_env = format!("{}", xdg.join("mise").display());
    // the waiter's own log, once it printed its outcome; a polled `print` repeated reads once
    let waited = |n: usize| {
        for _ in 0..200 {
            let lines = fs::read_to_string(&agent_log).unwrap_or_default();
            if lines.lines().count() >= n {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let mut log = norm(&fs::read_to_string(p.join("waiter.log")).unwrap_or_default());
        fs::remove_file(p.join("waiter.log")).ok();
        let mut seen: Vec<&str> = vec![];
        for l in log.lines() {
            if seen.last() != Some(&l) || !l.contains(" print ") {
                seen.push(l);
            }
        }
        log = seen.join("\n");
        (
            log,
            norm(&fs::read_to_string(&agent_log).unwrap_or_default()),
        )
    };
    let print = "launchctl print gui/$UID/dev.gigantic.m.upgrade";
    let reload = format!(
        "{print}\nlaunchctl bootout gui/$UID/dev.gigantic.m.upgrade\n{print}\nlaunchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist"
    );
    let reloaded = "hotswap: gui/$UID/dev.gigantic.m.upgrade reloaded from ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist\n";
    let versions = || {
        let mut v = (fs::read_dir(&ms).unwrap().flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        v.sort();
        format!(
            "{} → {}",
            v.join(" "),
            fs::read_link(ms.join("current")).unwrap().display()
        )
    };
    let plist = hotswap_plist(p);
    let smoke = "chk m\n";
    let mm = format!("~/.local/bin/m\n{print}\n");

    // a version mise holds that cannot be read stops the move, naming it, before anything moves
    on_mise();
    exe(&installs.join("0/m"), "");
    fs::set_permissions(installs.join("0/m"), fs::Permissions::from_mode(0o000)).unwrap();
    put_file("active", "2");
    assert!(by_shim(&["setup"], &xdg_env).starts_with("2\nm: cp: "));
    assert!(fs::read_link(ms.join("current")).is_err());
    fs::remove_dir_all(installs.join("0")).unwrap();

    // green: the new release's setup, run through the shim inside the old tick, moves the host —
    // XDG_DATA_HOME finds mise and moves nothing else — and the waiter reloads the agent from the
    // rewritten plist, its first bootstrap failing in launchd's window, retried; a pid file left by
    // a waiter killed at logout, its pid now another process's, does not stop it
    on_mise();
    put_file("once", "");
    put_file("active", "2");
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let reload_pid = p.join(".local/state/hotswap/m.reload");
    fs::write(&reload_pid, other.id().to_string()).unwrap();
    assert_eq!(by_shim(&["setup"], &xdg_env), format!("0\n{mm}"));
    other.kill().unwrap();
    other.wait().unwrap();
    let (log, outcome) = waited(1);
    assert_eq!(
        log,
        format!(
            "{reload}\nlaunchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist"
        )
    );
    assert_eq!(outcome, reloaded);
    assert_eq!(versions(), "1 2 current → 2");
    assert_eq!(fs::read(ms.join("2/m")).unwrap(), fs::read(&me).unwrap());
    assert_eq!(fs::read_link(&link).unwrap(), ms.join("current/m"));
    assert_eq!(fs::read_to_string(&agent).unwrap(), plist);
    assert!(installs.join("2/m").exists() && installs.join("1/m").exists());
    // every bootstrap failing leaves the agent unloaded and says so; the next setup loads it
    let from_layout = |args: &[&str]| launch(&mut Command::new(ms.join("2/m")), args, &[]);
    loaded(&shim, "not running");
    put_file("launchctl.bootstrap.code", "5");
    assert_eq!(from_layout(&["setup"]), format!("0\n{mm}"));
    let (_, outcome) = waited(2);
    let failed = "hotswap: gui/$UID/dev.gigantic.m.upgrade not reloaded from ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist after 5 tries: bootstrap out\n";
    assert_eq!(outcome, format!("{reloaded}{failed}"));
    assert_eq!(fs::read_to_string(&reload_pid).unwrap(), "unloaded\n");
    let boot = "launchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist\n";
    let disabled = "launchctl print-disabled gui/$UID\n";
    // a refused load is one line, here and in the agent's log; setup goes on, the mark stays
    let refused = "hotswap: gui/$UID/dev.gigantic.m.upgrade not loaded from ~/Library/LaunchAgents/dev.gigantic.m.upgrade.plist: bootstrap err\n";
    assert_eq!(
        from_layout(&["setup"]),
        format!("0\n~/.local/bin/m\n{refused}{print}\n{disabled}{boot}")
    );
    assert_eq!(waited(3).1, format!("{reloaded}{failed}{refused}"));
    assert_eq!(fs::read_to_string(&reload_pid).unwrap(), "unloaded\n");
    // a label the user disabled stays unloaded: no bootstrap, the mark cleared
    put_file(
        "launchctl.print-disabled.out",
        "\"dev.gigantic.m.upgrade\" => disabled\n",
    );
    assert_eq!(from_layout(&["setup"]), format!("0\n{mm}{disabled}"));
    assert!(!reload_pid.exists());
    fs::remove_file(p.join("launchctl.print-disabled.out")).unwrap();
    // loaded by nobody, not disabled: the next setup loads it
    fs::write(&reload_pid, "unloaded\n").unwrap();
    fs::remove_file(p.join("launchctl.bootstrap.code")).unwrap();
    assert_eq!(from_layout(&["setup"]), format!("0\n{mm}{disabled}{boot}"));
    assert!(!reload_pid.exists());
    // a mark left by a logout, the agent loaded again at login: cleared, no bootstrap
    loaded(&link, "not running");
    fs::write(&reload_pid, "unloaded\n").unwrap();
    assert_eq!(from_layout(&["setup"]), format!("0\n{mm}"));
    assert!(!reload_pid.exists());
    fs::write(&agent_log, "").unwrap();
    // the next tick, the agent on the link: no mise, nothing reloads
    loaded(&link, "not running");
    let tick = launch(&mut Command::new(&link), &["upgrade"], &[]);
    let m_latest =
        "curl -q -fsSIL -o /dev/null -w %{url_effective} https://github.com/x/m/releases/latest\n";
    assert_eq!(
        tick,
        format!("0\nm 2: green, 1 smoke probes\n{m_latest}{print}\n{smoke}")
    );
    // mise's newer 3 moves the layout on, a stale mise 2 never moves it back, and a run from the
    // layout relinks a link moved elsewhere
    copy_me("3");
    put_file("active", "3");
    assert_eq!(
        by_shim(&["setup"], &[("MISE_DATA_DIR", &mise_env)]),
        format!("0\n{mm}")
    );
    assert_eq!(versions(), "1 2 3 current → 3");
    put_file("active", "2");
    assert_eq!(
        by_shim(&["setup"], &[("MISE_DATA_DIR", &mise_env)]),
        format!("0\n{mm}")
    );
    assert_eq!(versions(), "1 2 3 current → 3");
    fs::remove_file(&link).unwrap();
    symlink(&shim, &link).unwrap();
    assert_eq!(
        launch(&mut Command::new(ms.join("3/m")), &["setup"], &[]),
        format!("0\n{mm}")
    );
    assert_eq!(fs::read_link(&link).unwrap(), ms.join("current/m"));
    // a `current` whose version was removed by hand is no current: the next run through mise's
    // shim repairs it
    fs::remove_dir_all(ms.join("3")).unwrap();
    put_file("active", "3");
    let fixed = by_shim(&["upgrade"], &[("MISE_DATA_DIR", &mise_env)]);
    assert!(
        fixed.starts_with("0\nm 3: green, 1 smoke probes\n"),
        "{fixed}"
    );
    assert_eq!(versions(), "2 3 current → 3");
    fs::remove_dir_all(installs.join("3")).unwrap();

    // a refused drain: the old tick bumped mise and stopped, so no setup ran and no layout exists;
    // the next tick, the old agent running the new binary through the shim, moves the host and
    // swaps — no hand-run setup
    // (that tick is the agent's running job, so its setup step finds the waiter already there)
    on_mise();
    fs::write(&agent_log, "").unwrap();
    loaded(&shim, "running");
    put_file("active", "2");
    let stranded = by_shim(&["upgrade"], &[("MISE_DATA_DIR", &mise_env)]);
    assert_eq!(
        stranded,
        format!("0\nm 2: green, 1 smoke probes\n{print}\n{m_latest}{print}\n{smoke}")
    );
    assert_eq!(versions(), "1 2 current → 2");
    assert_eq!(fs::read_link(&link).unwrap(), ms.join("current/m"));
    loaded(&shim, "not running");
    assert_eq!(waited(1), (reload.clone(), reloaded.into()));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(&agent_log).unwrap().lines().count(), 1);

    // smoke red, so the old tick `mise use`s 1, whose setup puts the link and the plist back on
    // the shim; the waiter, waiting out the old tick, loads the plist on disk: the old world,
    // whole
    on_mise();
    fs::write(&agent_log, "").unwrap();
    loaded(&shim, "running");
    put_file("active", "2");
    assert_eq!(by_shim(&["setup"], &xdg_env), format!("0\n{mm}"));
    put_file("active", "1");
    assert_eq!(by_shim(&["setup"], &[]), "0\nm@1 setup\n");
    loaded(&shim, "not running");
    let (log, outcome) = waited(1);
    assert_eq!(log, reload);
    assert_eq!(outcome, reloaded);
    assert_eq!(fs::read_link(&link).unwrap(), shim);
    assert_eq!(fs::read_to_string(&agent).unwrap(), "mise's\n");

    // the first release goes red after migrating: the old tick bumped mise to 2 and its drain
    // refused; the next tick, 2 through the shim, moves the host, swaps 2, goes red and rolls back
    // onto 1 — mise's copy of a pre-hotswap release — setting mise's active version back to 1
    // before 1's setup puts the link and the plist on the shim, so the shim runs 1, not red 2
    exe(
        &p.join("bin/mise"),
        &format!(
            "#!/bin/sh\necho \"mise $*\" >> '{0}/log'\n[ -e '{0}/mise.code' ] && echo 'mise: use refused' >&2 && exit 1\n[ \"$1\" = use ] && echo \"${{3##*@}}\" > '{0}/active'\nexit 0\n",
            p.display()
        ),
    );
    let record = || {
        let r: Value =
            serde_json::from_slice(&fs::read(p.join(".local/state/hotswap/m.json")).unwrap())
                .unwrap();
        let f = |k: &str| r[k].to_string();
        [f("version"), f("previous"), f("ok"), f("held"), f("red")].join(" ")
    };
    let active = || {
        fs::read_to_string(p.join("active"))
            .unwrap()
            .trim()
            .to_owned()
    };
    let mise = [("MISE_DATA_DIR", &*mise_env)];
    on_mise();
    fs::write(&agent_log, "").unwrap();
    put_file("tag", "1");
    put_file("active", "2");
    put_file("chk.m@2.code", "3");
    let rolled = by_shim(&["upgrade"], &mise);
    assert!(
        rolled.starts_with("1\nm: chk m failed; rolled back to 1\n")
            && rolled.contains("\nmise use -g packslip:x/m@1\nm@1 setup\n"),
        "{rolled}"
    );
    waited(1);
    loaded(&link, "not running");
    assert_eq!(versions(), "1 2 current → 1");
    assert_eq!(active(), "1");
    assert_eq!(fs::read_link(&link).unwrap(), shim);
    assert_eq!(fs::read_to_string(&agent).unwrap(), "mise's\n");
    assert_eq!(record(), r#""1" null false true "2""#);
    // had mise kept 2 (its `use` failing), the shim's 2 never moves `current` onto itself: red,
    // it is held, and --retry takes it again deliberately
    put_file("active", "2");
    assert_eq!(by_shim(&["setup"], &mise), format!("0\n{mm}"));
    assert_eq!(versions(), "1 2 current → 1");
    put_file("tag", "2");
    assert_eq!(
        by_shim(&["upgrade"], &mise),
        format!("1\nm: 2 is red; waiting for a newer release\n{m_latest}")
    );
    fs::remove_file(p.join("chk.m@2.code")).unwrap();
    put_file("next", "2");
    let retried = by_shim(&["upgrade", "--retry"], &mise);
    assert!(
        retried.starts_with("0\nm 2: green, 1 smoke probes\n"),
        "{retried}"
    );
    assert_eq!(versions(), "1 2 current → 2");
    // --back onto mise's 1 moves mise too; a refused `mise use` is a recorded step, not a stop
    put_file("mise.code", "1");
    let back = by_shim(&["upgrade", "--back"], &mise);
    assert!(
        back.starts_with("0\nm 1: green, 2 smoke probes\n"),
        "{back}"
    );
    let smoke: Value =
        serde_json::from_slice(&fs::read(p.join(".local/state/hotswap/m.json")).unwrap()).unwrap();
    assert_eq!(
        (&smoke["smoke"][0]["probe"], &smoke["smoke"][0]["code"]),
        (&json!("mise use -g packslip:x/m@1"), &json!(1))
    );
    assert_eq!(versions(), "1 2 current → 1");
    fs::remove_file(p.join("mise.code")).unwrap();
    // the loop closes: back on mise, the old release's tick bumps mise to 3 and runs 3's setup
    // through the shim, which moves the host onto 3 past the stale 2
    copy_me("3");
    put_file("active", "3");
    assert_eq!(by_shim(&["setup"], &mise), format!("0\n{mm}"));
    assert_eq!(versions(), "1 2 3 current → 3");
    assert_eq!(fs::read_link(&link).unwrap(), ms.join("current/m"));
}

/// `agent_plist("m", "m")` under `p`, escaped as the crate escapes it.
fn hotswap_plist(p: &Path) -> String {
    let home = p.display().to_string().replace('&', "&amp;");
    let home = home.replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.gigantic.m.upgrade</string>
  <key>ProgramArguments</key>
  <array>
    <string>{home}/.local/bin/m</string>
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
  <key>StandardOutPath</key><string>{home}/Library/Logs/dev.gigantic.m.upgrade.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/dev.gigantic.m.upgrade.log</string>
</dict>
</plist>
"#
    )
}
