//! The suite is also the tool: run under a tool's name (argv[0], never a path) it is that tool's
//! `upgrade` — or, given `setup`, its `hotswap::ensure` — linking hotswap with the manifest named
//! for it below, or in `MANIFEST`; run bare by cargo, it drives itself through every branch, and
//! with `--ignored` (`cargo test -- --ignored`) through the real packslip and real grove releases.
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
    let tool = args.next().unwrap_or_default();
    let m = json!({
        "bare": {"repo": "x/bare"},
        "gone": {"bin": "g", "repo": "x/g", "smoke": [["absent"]]},
        "slow": {"repo": "x/slow", "smoke": [["sh", "-c", "sleep 3 & echo up"], ["sh", "-c", "(sleep 2; touch ~/m) & sleep 3"]], "timeout": 1},
        "t": {"agents": ["dev.gigantic.t.upgrade", "dev.gigantic.t"], "drain": [["t", "drain"]], "repo": "x/t", "setup": ["t", "setup"], "signer": "ps1_t", "smoke": [["t", "version"], ["chk", "x"]]},
    });
    let given = env::var("MANIFEST")
        .ok()
        .map(|j| serde_json::from_str(&j).unwrap());
    let of = |t: &str| {
        serde_json::from_value(given.clone().or(m.get(t).cloned()).unwrap_or(json!({}))).unwrap()
    };
    let args = args.collect::<Vec<_>>();
    match tool.as_str() {
        "forget" => hotswap::forget("t"),
        "install" => exit(hotswap::install("t", &of("t"))),
        "plist" => print!("{}", hotswap::agent_plist("t", "tb")),
        t if !t.contains('/') && args == ["setup"] => match hotswap::ensure(t, &of(t)) {
            Ok(p) => println!("{}", p.display()),
            Err(e) => {
                eprintln!("{e}");
                exit(2)
            }
        },
        t if !t.contains('/') => exit(hotswap::upgrade(t, &of(t), &args)),
        _ if args.iter().any(|a| a == "--ignored") => real(),
        _ => suite(),
    }
}

/// The real thing, in a scratch HOME with launchctl stubbed: bootstrap the pinned packslip and
/// install the latest grove, upgrade onto it from an older real release (pruning the oldest),
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
    let grove = |smoke: &str, signer: &str, args: &[&str]| {
        let manifest =
            json!({"repo": "jgeschwendt/grove", "signer": signer, "smoke": [["sh", "-c", smoke]]});
        let started = Instant::now();
        let o = (Command::new(env::current_exe().unwrap())
            .arg0("grove")
            .args(args))
        .env("HOME", p)
        .env("PATH", &path)
        .env("MANIFEST", manifest.to_string())
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
        let out = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
        println!(
            "grove {}: {:.1?}, exit {:?}\n  {}",
            args.join(" "),
            started.elapsed(),
            o.status.code(),
            out.trim().replace('\n', "\n  ")
        );
        (o.status.code().unwrap_or(-1), out)
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
    // a first install: packslip bootstrapped by its pinned sha256, the latest grove relocated into
    // the layout and run from there; packslip's export stays off ~/.local/bin
    assert_eq!(grove("true", signer, &["setup"]).0, 0);
    let latest = current();
    assert_eq!(version(), format!("grove {latest}"));
    let local = fs::read_dir(p.join(".local/bin"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect::<Vec<_>>();
    assert_eq!(local, ["grove"]);
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

// One host walked through every branch. `curl`, `launchctl`, `sha256sum`, `uname` and `chk` are
// one sh stub that logs its argv and prints `<name>.<verb>.out`, else `<name>.out`, else `<verb>
// out`; `<name>.code`, `<name>.<verb>.code` or `<name>@<active version>.code` makes it exit
// non-zero, and `quiet` silences its stderr. curl's download writes the packslip stub, which
// unpacks the tool stub as version `next`, with `bin/<bin>` when the bin is not `t`; the tool stub
// logs `<bin>@<its version>`, so a log shows which version each step ran, and a one-shot
// `crash.<version>` makes its `setup` SIGKILL its caller. `old/t` leads PATH, as a shim leaves the
// running install's bin dir: a step that runs it fails the exact log match. The temp dir's `&<>`
// must be escaped in the plist.
fn suite() {
    let d = tempfile::Builder::new().prefix("hot&<>").tempdir().unwrap();
    let p = d.path();
    let (bin, old, root) = (p.join("bin"), p.join("old"), p.join(".local/share/hotswap"));
    let ts = root.join("t");
    let stub = format!(
        "#!/bin/sh\nn=${{0##*/}}; v=$(readlink '{t}/current')\necho \"$n $*\" >> '{p}/log'\n[ \"$n $1\" = 'curl -fsSL' ] && cp '{p}/packslip.stub' \"$3\"\ncat '{p}'/\"$n.$1.out\" 2>/dev/null || cat '{p}'/\"$n.out\" 2>/dev/null || echo \"$1 out\"; [ -e '{p}/quiet' ] || echo \"$1 err\" >&2\nexit $(cat \"{p}/$n.code\" 2>/dev/null || cat \"{p}/$n.$1.code\" 2>/dev/null || cat \"{p}/$n@$v.code\" 2>/dev/null || echo 0)\n",
        t = ts.display(),
        p = p.display()
    );
    let tool = format!(
        "#!/bin/sh\nn=${{0##*/}}; r=$(readlink -f \"$0\"); r=${{r#*/hotswap/*/}}; v=${{r%%/*}}\necho \"$n@$v $*\" >> '{0}/log'\n[ \"$1\" = setup ] && rm \"{0}/crash.$v\" 2>/dev/null && kill -9 $PPID\necho \"$1 out\"; [ -e '{0}/quiet' ] || echo \"$1 err\" >&2\nexit $(cat \"{0}/$n.code\" 2>/dev/null || cat \"{0}/$n.$1.code\" 2>/dev/null || echo 0)\n",
        p.display()
    );
    let packslip = format!(
        "#!/bin/sh\necho \"packslip $* state=$XDG_STATE_HOME\" >> '{0}/log'\n[ -e '{0}/packslip.code' ] && printf 'Error: \\n   0: \\033[91mno release\\033[0m\\n\\nLocation:\\n' >&2 && exit 1\nn=${{2##*/}}; rel=$n; [ \"$n\" = t ] || rel=bin/$n\nmkdir -p \"$(dirname \"$6/$rel\")\" && cp '{0}/tool.stub' \"$6/$rel\" && ln -sf \"$(cd \"$6\" && pwd -P)/$rel\" \"$8/$n\"\necho \"Installed $2 $(cat '{0}/next') at $6\"\n",
        p.display()
    );
    let exe = |path: &Path, body: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    for name in ["chk", "curl", "launchctl", "sha256sum", "uname"] {
        exe(&bin.join(name), &stub);
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
    let tag = |v: &str| {
        put_file(
            "curl.out",
            &format!("https://github.com/x/t/releases/tag/v{v}"),
        );
    };
    put_file("uname.out", "Darwin arm64\n");
    let sum = "2b21dc68d54c23a56123f9f9d7402a8b9cfb1e6950c5d4966730ec5cee20146b";
    put_file("sha256sum.out", &format!("{sum}  x\n"));
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
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
        format!("{}\n{out}{log}", o.status.code().unwrap_or(-1))
            .replace(&format!("gui/{}", uid.trim()), "gui/$UID")
            .replace(&p.display().to_string(), "~")
    };
    let go = |tool: &str, args: &[&str], envs: &[(&str, &str)]| {
        launch(
            Command::new(env::current_exe().unwrap()).arg0(tool),
            args,
            envs,
        )
    };
    let run = |args: &[&str]| go("t", args, &[]);
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
    let latest =
        "curl -fsSI -o /dev/null -w %{redirect_url} https://github.com/x/t/releases/latest\n";
    let pk = |tool: &str, repo: &str, pin: &str| {
        format!(
            "packslip install github.com/{repo} --pin {pin} --install-dir ~/.local/share/hotswap/stage/{tool} --bin-dir ~/.local/share/hotswap/packslip/bin state=~/.local/share/hotswap/packslip/state\n"
        )
    };
    let fetch = |tool: &str, repo: &str, pin: &str| format!("uname -sm\n{}", pk(tool, repo, pin));
    let t_fetch = fetch("t", "x/t", "ps1_t");
    let t_pk = pk("t", "x/t", "ps1_t");
    let print = "launchctl print gui/$UID/dev.gigantic.t.upgrade\n";
    let steps = |v: &str| {
        format!(
            "t@{v} setup\nlaunchctl kickstart -k gui/$UID/dev.gigantic.t\nt@{v} version\nchk x\n"
        )
    };
    // the drain runs on the active version, before `current` moves to the new one
    let swap = |from: &str, to: &str| format!("t@{from} drain\n{}", steps(to));
    let red = |why: &str, log: &str| format!("1\nt: chk x failed; {why}\n{latest}{log}");

    let usage = "2\nusage: t upgrade [--check | --back | --retry]\n";
    assert_eq!(run(&["--x"]), usage);
    assert_eq!(run(&["--back", "--check"]), usage);
    assert_eq!(go("gone", &["--x"], &[]), usage.replace(" t ", " g "));
    assert_eq!(go("t", &[], &[("HOME", "")]), "2\nHOME unset\n");
    assert_eq!(
        run(&[]),
        "2\nt: no version under ~/.local/share/hotswap/t; run t setup\n"
    );

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
    // failing its pinned sha256, then bootstrapped, its install failing, and through
    let uname = "uname -sm\n";
    let ps = root.join("packslip/1.11.4/packslip");
    let dl = "curl -fsSL -o ~/.local/share/hotswap/packslip/1.11.4/packslip.tmp https://packslip.dev/v1.11.4/packslip-v1.11.4-darwin-arm64\n";
    put_file("uname.out", "Darwin x86_64\n");
    assert_eq!(
        go("install", &[], &[]),
        format!(
            "2\nt: Darwin x86_64 is unsupported: packslip builds darwin-arm64, linux-arm64 and linux-x64\n{uname}"
        )
    );
    put_file("uname.out", "Darwin arm64\n");
    code("curl.code");
    let boot = "2\nt: packslip 1.11.4 bootstrap: ";
    assert_eq!(
        go("install", &[], &[]),
        format!("{boot}-fsSL err\n{uname}{dl}")
    );
    rm("curl.code");
    put_file("sha256sum.out", "bad  x\n");
    let sha = "sha256sum ~/.local/share/hotswap/packslip/1.11.4/packslip.tmp\n";
    assert_eq!(
        go("install", &[], &[]),
        format!(
            "{boot}https://packslip.dev/v1.11.4/packslip-v1.11.4-darwin-arm64 is not the pinned sha256 {sum}\n{uname}{dl}{sha}"
        )
    );
    assert!(!ps.with_extension("tmp").exists());
    put_file("sha256sum.out", &format!("{sum}  x\n"));
    code("packslip.code");
    assert_eq!(
        go("install", &[], &[]),
        format!("2\nt: packslip install failed: no release\n{uname}{dl}{sha}{t_pk}")
    );
    assert_eq!(
        fs::metadata(&ps).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fs::set_permissions(&ps, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        go("install", &[], &[]),
        format!("2\nt: packslip install failed: Permission denied (os error 13)\n{uname}")
    );
    fs::set_permissions(&ps, fs::Permissions::from_mode(0o755)).unwrap();
    rm("packslip.code");
    put_file("next", "2");
    assert_eq!(
        go("install", &[], &[]),
        format!("0\nsetup out\nsetup err\n{t_fetch}{print}t@2 setup\n")
    );
    assert_eq!(versions(&ts), "2*");
    assert_eq!(
        fs::read_link(p.join(".local/bin/t")).unwrap(),
        ts.join("current/t")
    );
    assert!(!root.join("stage/t").exists());
    // a second install fetches nothing: setup alone, its failure exit 1
    code("t.setup.code");
    assert_eq!(
        go("install", &[], &[]),
        format!("1\nsetup out\nsetup err\n{print}t@2 setup\n")
    );
    rm("t.setup.code");
    assert_eq!(run(&["setup"]), format!("0\n~/.local/bin/t\n{print}"));

    // the latest check failing, then naming no release: nothing tried
    code("curl.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: https://github.com/x/t/releases/latest: -fsSI err\n{latest}")
    );
    rm("curl.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: https://github.com/x/t/releases/latest names no release\n{latest}")
    );
    tag("2");
    assert_eq!(run(&["--back"]), "2\nt: no previous version\n");
    let plan = swap("2", "2")
        .trim_end()
        .replace('\n', "\n  ")
        .replace("t@2 ", "t ");
    assert_eq!(
        run(&["--check"]),
        format!("0\nt: none → 2\n  {plan}\n{latest}")
    );

    // red with no previous version: stays put, held while it is latest
    code("chk.code");
    assert_eq!(run(&[]), red("no previous version", &swap("2", "2")));
    assert_eq!(state(), r#""2" null false "chk x" "2""#);
    let wait = "t: 2 is red; waiting for a newer release";
    assert_eq!(run(&[]), format!("1\n{wait}\n{latest}"));
    assert_eq!(run(&["--check"]), format!("0\n{wait}\n{latest}"));
    rm("chk.code");

    // a newer release: packslip's install failing changes nothing; a refused drain leaves the
    // active version active, the fetched one beside it
    tag("3");
    put_file("next", "3");
    code("packslip.code");
    assert_eq!(
        run(&[]),
        format!("2\nt: packslip install failed: no release\n{latest}{t_fetch}")
    );
    assert_eq!(state(), r#""2" null false "chk x" "2""#);
    rm("packslip.code");
    code("t.drain.code");
    assert_eq!(
        run(&[]),
        format!("1\nt: t drain refused; nothing swapped\n{latest}{t_fetch}t@2 drain\n")
    );
    assert_eq!(versions(&ts), "2* 3");
    rm("t.drain.code");

    // green on 3: the fetch is a no-op once 3 is on disk, and the green prunes past previous
    put("2");
    lay("1");
    assert_eq!(
        run(&[]),
        format!(
            "0\nt 3: green, 2 smoke probes\n{latest}{t_fetch}{}",
            swap("2", "3")
        )
    );
    assert_eq!(state(), r#""3" "2" true null null"#);
    assert_eq!(versions(&ts), "2 3*");
    let smoke = json!([{"code": 0, "output": "version out\nversion err\n", "probe": "t version"}, {"code": 0, "output": "x out\nx err\n", "probe": "chk x"}]);
    assert_eq!(rec("t")["smoke"], smoke);
    assert_eq!(run(&[]), format!("0\nt 3: up to date\n{latest}"));
    assert_eq!(run(&["--check"]), format!("0\nt 3: up to date\n{latest}"));
    // a newer release in --check is reported, not fetched
    tag("4");
    let plan = swap("3", "3")
        .trim_end()
        .replace('\n', "\n  ")
        .replace("t@3 ", "t ");
    assert_eq!(
        run(&["--check"]),
        format!("0\nt: 3 → 4\n  {plan}\n{latest}")
    );
    // packslip skips a withdrawn 4 and chooses 3: nothing newer, nothing kept
    assert_eq!(run(&[]), format!("0\nt 3: up to date\n{latest}{t_fetch}"));
    assert_eq!(versions(&ts), "2 3*");
    assert!(!root.join("stage/t").exists());
    assert_eq!(
        run(&["--back"]),
        format!("0\nt 2: green, 2 smoke probes\n{}", steps("2"))
    );
    assert_eq!(state(), r#""2" "3" true null "3""#);

    // red with a previous version: roll back with no network, keeping the record's previous
    tag("2");
    lay("1");
    put("1");
    code("chk@2.code");
    let rollback = steps("1");
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
    assert_eq!(run(&[]), format!("1\n{wait}\n{latest}"));
    assert_eq!(run(&["--check"]), format!("0\n{wait}\n{latest}"));
    put_file("next", "2");
    assert_eq!(
        run(&["--retry"]),
        red(
            "rolled back to 1",
            &format!("{t_fetch}{}{rollback}", swap("1", "2"))
        )
    );
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
            "1\nt: {} failed; rolled back to 1, where {} failed too\n{latest}t@3 drain\n",
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
        format!("-1\n{latest}{}t@2 setup\n", swap("3", "3"))
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
    rm("chk@1.code");
    rm("chk.code");
    rm("chk@2.code");
    rm("chk@3.code");

    // another upgrade holds the lock: refused, but --check reads on,
    let held = File::create(state_dir.join("t.lock")).unwrap();
    held.try_lock().unwrap();
    assert_eq!(run(&[]), "2\nt: another hotswap is running\n");
    // and a red release that is no longer latest stays red, settled
    tag("1");
    assert_eq!(run(&["--check"]), format!("0\nt 1: red at chk x\n{latest}"));
    drop(held);
    assert_eq!(run(&[]), format!("1\nt 1: red at chk x\n{latest}"));

    // a probe past its deadline is a red probe and its whole group dies; one whose background child
    // holds the pipe keeps its code
    let setup = |t: &str, repo: &str, bin: &str| {
        let label = format!("launchctl print gui/$UID/dev.gigantic.{t}.upgrade\n");
        assert_eq!(
            go(t, &["setup"], &[]),
            format!("0\n~/.local/bin/{bin}\n{}{label}", fetch(t, repo, ""))
        );
    };
    put_file("next", "2");
    setup("slow", "x/slow", "slow");
    let late = "sh -c (sleep 2; touch ~/m) & sleep 3";
    assert_eq!(
        go("slow", &[], &[]),
        format!("1\nslow: {late} failed; no previous version\n{latest}").replace("x/t", "x/slow")
    );
    assert_eq!(
        rec("slow")["smoke"],
        json!([{"code": 0, "output": "output held open past 1s", "probe": "sh -c sleep 3 & echo up"}, {"code": -1, "output": "timed out after 1s", "probe": late}])
    );
    thread::sleep(Duration::from_secs(2));
    assert!(!p.join("m").exists());

    // a missing command is a red probe, and a release keeping its binary deeper gets a link at its
    // top; an unwritable lock or record exits 2
    setup("gone", "x/g", "g");
    assert_eq!(
        fs::read_link(root.join("gone/2/g")).unwrap(),
        Path::new("bin/g")
    );
    assert_eq!(
        go("gone", &[], &[]),
        format!("1\ngone: absent failed; no previous version\n{latest}").replace("x/t", "x/g")
    );
    assert_eq!(rec("gone")["smoke"][0]["code"], -1);
    setup("bare", "x/bare", "bare");
    let bare = format!("2\nbare: {}.", state_dir.join("bare").display())
        .replace(&*p.display().to_string(), "~");
    for f in ["bare.lock", "bare.tmp"] {
        fs::create_dir(state_dir.join(f)).unwrap();
        assert!(go("bare", &[], &[]).starts_with(&bare));
        fs::remove_dir(state_dir.join("bare.lock")).ok();
    }

    // forget drops the record, the lock and the versions, absent or not
    for _ in 0..2 {
        assert_eq!(go("forget", &[], &[]), "0\n");
    }
    assert!(!state_dir.join("t.json").exists() && !state_dir.join("t.lock").exists());
    assert!(!ts.exists());

    // The move off mise: `~/.local/bin/t` on mise's shim, the upgrade agent loaded from its plist
    // on that shim, versions 1 and 2 in mise's installs, 2 this very binary. Run as mise's 2 —
    // the new release's `setup`, under the old release's upgrade agent — it copies both into the
    // layout offline, points `current` at 2 and the link through it, rewrites the agent's plist,
    // and reloads the agent once its run ends.
    let mise = p.join(".local/share/mise");
    let installs = mise.join("installs/packslip-jgeschwendt-t");
    let shim = mise.join("shims/t");
    let me = env::current_exe().unwrap();
    exe(&shim, "#!/bin/sh\n");
    exe(&installs.join("1/t"), &tool);
    let copy_me = |v: &str| {
        fs::create_dir_all(installs.join(v)).unwrap();
        fs::copy(&me, installs.join(v).join("t")).unwrap();
        installs.join(v).join("t")
    };
    let two = copy_me("2");
    fs::create_dir_all(installs.join(".mise")).unwrap();
    symlink("./2", installs.join("global")).unwrap();
    fs::remove_file(p.join(".local/bin/t")).unwrap();
    symlink(&shim, p.join(".local/bin/t")).unwrap();
    let agent = p.join("Library/LaunchAgents/dev.gigantic.t.upgrade.plist");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    put_file("stale", "mise's");
    fs::copy(p.join("stale"), &agent).unwrap();
    let loaded = |program: &Path| {
        put_file(
            "launchctl.print.out",
            &format!(
                "path = {}\nstate = not running\nprogram = {}\n",
                agent.display(),
                program.display()
            ),
        );
    };
    loaded(&shim);
    let from = |x: &Path| launch(Command::new(x).arg0("t"), &["setup"], &[]);
    // a version mise holds that cannot be read stops the move, naming it
    exe(&installs.join("0/t"), "");
    fs::set_permissions(installs.join("0/t"), fs::Permissions::from_mode(0o000)).unwrap();
    assert!(from(&two).starts_with("2\nt: cp: "));
    fs::remove_dir_all(installs.join("0")).unwrap();
    let o = (Command::new(&two)
        .arg0("t")
        .arg("setup")
        .env("HOME", p)
        .env("PATH", &path))
    .env_remove("MISE_DATA_DIR")
    .env_remove("XDG_DATA_HOME")
    .output()
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&[o.stdout, o.stderr].concat()),
        format!("{}\n", p.join(".local/bin/t").display())
    );
    let reload = "launchctl bootout gui/$UID/dev.gigantic.t.upgrade\nlaunchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.gigantic.t.upgrade.plist\n";
    let mut log = String::new();
    for _ in 0..100 {
        log = fs::read_to_string(p.join("log")).unwrap_or_default();
        if log.contains("bootstrap") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    fs::remove_file(p.join("log")).ok();
    assert_eq!(
        log.replace(&format!("gui/{}", uid.trim()), "gui/$UID")
            .replace(&p.display().to_string(), "~"),
        format!("{print}{print}{reload}")
    );
    assert_eq!(versions(&ts), "1 2*");
    assert_eq!(fs::read(ts.join("2/t")).unwrap(), fs::read(&me).unwrap());
    assert_eq!(
        fs::read_link(p.join(".local/bin/t")).unwrap(),
        ts.join("current/t")
    );
    assert_eq!(
        fs::read_to_string(&agent).unwrap(),
        plist.replace("/tb<", "/t<")
    );
    // mise's dirs are left alone
    assert!(two.exists() && installs.join("1/t").exists() && installs.join("global").exists());
    // again from mise's 2, the agent now on the link: nothing moves, nothing reloads; mise's
    // newer 3 moves the layout to 3, and a stale mise 2 never moves it back
    loaded(&p.join(".local/bin/t"));
    assert_eq!(from(&two), format!("0\n~/.local/bin/t\n{print}"));
    assert_eq!(versions(&ts), "1 2*");
    assert_eq!(from(&copy_me("3")), format!("0\n~/.local/bin/t\n{print}"));
    assert_eq!(versions(&ts), "1 2 3*");
    assert_eq!(from(&two), format!("0\n~/.local/bin/t\n{print}"));
    assert_eq!(versions(&ts), "1 2 3*");
    // run from the layout, not mise: it relinks a link moved elsewhere
    fs::remove_file(p.join(".local/bin/t")).unwrap();
    symlink(&shim, p.join(".local/bin/t")).unwrap();
    assert_eq!(from(&ts.join("3/t")), format!("0\n~/.local/bin/t\n{print}"));
    assert_eq!(
        fs::read_link(p.join(".local/bin/t")).unwrap(),
        ts.join("current/t")
    );
}
