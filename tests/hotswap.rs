use serde_json::{Value, json};
use std::{
    fs::{self, File},
    os::unix::fs::{PermissionsExt, symlink},
    process::Command,
};

// One host walked through every branch: `mise`, `launchctl` and the tool's commands are one sh stub
// that logs its argv; `<name>.code` makes that stub exit non-zero, and `mise use` clears those
// unless `sticky` exists, `quiet` silences its stderr, and a one-shot `crash` SIGKILLs its caller. `mise ls` answers only from
// `/`; any other mise call moves `next.json` over `ls.json`; `mise latest <key>`, unlogged since
// they run in parallel, sources `latest/<key>` or fails silently. The temp dir's `&<>` must be
// escaped in the plist.
#[test]
fn hotswap() {
    let d = tempfile::Builder::new().prefix("hot&<>").tempdir().unwrap();
    let p = d.path();
    let bin = p.join("bin");
    for (name, body) in [
        ("bad/hotswap.toml", "agents = 1"),
        ("gone/hotswap.toml", r#"smoke = [["absent"]]"#),
        (
            "latest/packslip:jgeschwendt/lost",
            "echo no release found >&2; echo run with --verbose >&2",
        ),
        (
            "latest/packslip:jgeschwendt/t",
            "echo ' WARN  mise: noise'; echo 3; echo 'Version: 9'; exit 0",
        ),
        (
            "latest/packslip:jgeschwendt/t1",
            "echo ' WARN  x' >&2; exit 0",
        ),
        (
            "slow/hotswap.toml",
            "smoke = [[\"sh\", \"-c\", \"sleep 3 & echo up\"], [\"sh\", \"-c\", \"(sleep 2; touch ~/m) & sleep 3\"]]\ntimeout = 1",
        ),
        (
            "t/hotswap.toml",
            r#"agents = ["dev.gigantic.hotswap", "dev.gigantic.t"]
drain = [["t", "drain"]]
setup = ["t", "setup"]
smoke = [["t", "version"], ["chk", "x"]]"#,
        ),
        ("t/mise", "not executable, so setup looks further"),
        ("t1/hotswap.toml", r#"smoke = [["chk", "one"]]"#),
    ] {
        fs::create_dir_all(p.join(name).parent().unwrap()).unwrap();
        fs::write(p.join(name), body).unwrap();
    }
    let shims = p.join(".local/share/mise/shims");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&shims).unwrap();
    // `t/hotswap` leads PATH, as an old install's bin dir does under a shim; a tick that runs it fails
    // the exact log match
    for name in ["bin/chk", "bin/launchctl", "bin/mise", "bin/t", "t/hotswap"] {
        let stub = format!(
            "#!/bin/sh\nn=${{0##*/}}\n[ \"$n $1\" = 'mise latest' ] && {{ [ -e \"{0}/latest/$2\" ] && . \"{0}/latest/$2\"; exit 1; }}\necho \"$n $*\" >> '{0}/log'\n[ \"$n $1\" = 'mise ls' ] && [ \"$(pwd -P)\" = / ] && cd '{0}' && exec cat ls.json\n[ \"$n\" = mise ] && mv '{0}/next.json' '{0}/ls.json' 2>/dev/null\n[ \"$n $1\" = 'mise use' ] && rm '{0}/crash' 2>/dev/null && kill -9 $PPID\n[ \"$n $1\" = 'mise use' ] && [ ! -e '{0}/sticky' ] && rm -f '{0}'/*.code\necho \"$1 out\"; [ -e '{0}/quiet' ] || echo \"$1 err\" >&2\nexit $(cat \"{0}/$n.code\" 2>/dev/null || echo 0)\n",
            p.display()
        );
        fs::write(p.join(name), stub).unwrap();
        fs::set_permissions(p.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    symlink(env!("CARGO_BIN_EXE_hotswap"), shims.join("hotswap")).unwrap();
    // `t` 1 lives apart with its own manifest; `v` is the active one of 2 and 3
    let ls = |v: &str| {
        let e = |dir: &str, active: bool, installed: bool, version: &str| json!({"active": active, "install_path": p.join(dir), "installed": installed, "version": version});
        let one = |dir, active, installed| json!([e(dir, active, installed, "1")]);
        let t = json!([
            e("t1", false, true, "1"),
            e("t", v == "2", true, "2"),
            e("t", v == "3", true, "3")
        ]);
        let all = json!({"aqua:x/bad": one("bad", true, true), "aqua:x/t": one("t", false, true), "bare": one("bare", true, true), "cargo:gone": one("gone", true, true), "idle": one("t", false, true), "lost": one("t", true, false), "packslip:jgeschwendt/install": one("t", v != "1", true), "packslip:jgeschwendt/ls": one("t", v != "1", true), "packslip:jgeschwendt/n": one("t", v == "n", true), "packslip:jgeschwendt/remove": one("t", v != "1", true), "packslip:jgeschwendt/t": t, "packslip:jgeschwendt/setup": one("t", v != "1", true), "packslip:jgeschwendt/t1": one("t1", v != "1", true), "packslip:jgeschwendt/tick": one("t", v != "1", true), "packslip:jgeschwendt/upgrade": one("t", v != "1", true), "slow": one("slow", true, true), "two": [e("t", true, true, "1"), e("t", true, true, "2")]});
        fs::write(p.join("ls.json"), all.to_string()).unwrap();
    };
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    // exit code, stdout + stderr, then the stubs' log
    let go = |args: &[&str], envs: &[(&str, &str)]| {
        let path = format!("{}/t:{}:/bin:/usr/bin", p.display(), bin.display());
        let o = Command::new(env!("CARGO_BIN_EXE_hotswap"))
            .args(args)
            .env("HOME", p)
            .env("PATH", path)
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
    };
    let run = |args: &[&str]| go(args, &[]);
    let state_dir = p.join(".local/state/hotswap");
    let rec = |tool: &str| {
        serde_json::from_slice::<Value>(&fs::read(state_dir.join(format!("{tool}.json"))).unwrap())
            .unwrap()
    };
    let state = || {
        let r = rec("t");
        format!(
            "{} {} {} {}",
            r["version"], r["previous"], r["ok"], r["failed"]
        )
    };
    let probes = || {
        let r = rec("t");
        let s = r["smoke"].as_array().unwrap().iter();
        s.map(|s| format!("{} {}", s["probe"], s["code"]))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // a green record at `v`, whose previous is 0 — a version mise no longer holds
    let put = |v: &str| {
        let r = json!({"at": 0, "ok": true, "previous": "0", "smoke": [], "version": v});
        fs::write(state_dir.join("t.json"), r.to_string()).unwrap();
    };
    let code = |name: &str| fs::write(p.join(name), "3").unwrap();
    let usage = "2\nusage: hotswap <tool> [--check | --back] | hotswap ls [<tool>…] | hotswap install <tool> | hotswap upgrade <tool> | hotswap remove <tool> | hotswap setup [--remove] | hotswap tick\n";
    let ls_log = "mise ls --json\n";
    // a run that stops after the `mise ls` lookup, printing `want`
    let ls_only = |args: &[&str], want: &str| assert_eq!(run(args), format!("{want}\n{ls_log}"));
    let swap_log =
        "t drain\nt setup\nlaunchctl kickstart -k gui/$UID/dev.gigantic.t\nt version\nchk x\n";
    let use_log = |v: &str| format!("mise use -g packslip:jgeschwendt/t@{v}\n");
    let rollback = format!("{}chk one\n", use_log("1"));
    let back_log = |v: &str| format!("{}{}", use_log(v), &swap_log[8..]);
    let red =
        |why: &str, rest: &str| format!("1\nt: chk x failed; {why}\n{ls_log}{swap_log}{rest}");

    assert_eq!(run(&[]), usage);
    assert_eq!(run(&["--back"]), usage);
    assert_eq!(run(&["t", "--x"]), usage);
    assert_eq!(run(&["install"]), usage);
    assert_eq!(run(&["upgrade", "--back"]), usage);
    assert_eq!(go(&["t"], &[("HOME", "")]), "2\nHOME unset\n");

    // setup: shims from MISE_DATA_DIR, XDG_DATA_HOME, then HOME; unchanged; a failing launchctl
    // reloads and reports; --remove reports what it undid; an unwritable plist exits 2
    let agent = p.join("Library/LaunchAgents/dev.gigantic.hotswap.plist");
    let a = agent.display();
    let bootout = "launchctl bootout gui/$UID/dev.gigantic.hotswap\n";
    let reload = format!("{bootout}launchctl bootstrap gui/$UID {a}\n");
    let print = "launchctl print gui/$UID/dev.gigantic.hotswap\n";
    let fresh =
        format!("0\nhotswap setup: {a} written; dev.gigantic.hotswap loaded, every 900s\n{reload}");
    let (bins, home) = (bin.display(), p.display());
    let plist = |shims: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.gigantic.hotswap</string>
  <key>ProgramArguments</key>
  <array>
    <string>{shims}/hotswap</string>
    <string>tick</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{shims}:{bins}:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StartInterval</key><integer>900</integer>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{home}/Library/Logs/dev.gigantic.hotswap.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/dev.gigantic.hotswap.log</string>
</dict>
</plist>
"#
        )
        .replace("hot&<>", "hot&amp;&lt;&gt;")
    };
    for (envs, shims) in [
        (
            &[("MISE_DATA_DIR", "/m"), ("XDG_DATA_HOME", "/x")][..],
            "/m/shims".into(),
        ),
        (&[("XDG_DATA_HOME", "/x")], "/x/mise/shims".into()),
        (&[], format!("{home}/.local/share/mise/shims")),
    ] {
        assert_eq!(go(&["setup"], envs), fresh);
        assert_eq!(fs::read_to_string(&agent).unwrap(), plist(&shims));
    }
    let same = "hotswap setup: dev.gigantic.hotswap unchanged";
    assert_eq!(run(&["setup"]), format!("0\n{same}\n{print}"));
    code("launchctl.code");
    let failed = format!("hotswap setup: launchctl bootstrap gui/$UID {a} failed");
    assert_eq!(run(&["setup"]), format!("1\n{failed}\n{print}{reload}"));
    let undo = ["setup", "--remove"];
    let undone = |out: &str, rm: &str| {
        format!("0\nhotswap setup --remove: dev.gigantic.hotswap {out}; {a} {rm}\n{bootout}")
    };
    assert_eq!(run(&undo), undone("not loaded", "removed"));
    fs::create_dir(&agent).unwrap();
    assert!(run(&["setup"]).starts_with(&format!("2\nhotswap setup: {a}: Is a directory")));
    fs::remove_dir(&agent).unwrap();
    fs::remove_file(p.join("launchctl.code")).unwrap();
    assert_eq!(run(&undo), undone("booted out", "absent"));

    // mise missing, failing, then talking nonsense
    ls_only(&["t"], "2\nmise: cat: ls.json: No such file or directory");
    fs::write(p.join("ls.json"), "x").unwrap();
    ls_only(&["t"], "2\nmise: expected value at line 1 column 1");
    fs::rename(bin.join("mise"), p.join("mise")).unwrap();
    assert_eq!(run(&["t"]), "2\nmise: Permission denied (os error 13)\n");
    assert_eq!(run(&["setup"]), "2\nhotswap setup: no mise on PATH\n");
    fs::rename(p.join("mise"), bin.join("mise")).unwrap();
    ls("2");

    for msg in [
        "idle: no active mise install",
        "lost 1: not installed",
        "nope: no active mise install",
        "two: several active versions",
    ] {
        let tool = msg.split([' ', ':']).next().unwrap();
        ls_only(&[tool], &format!("2\n{msg}"));
    }
    assert!(run(&["bad"]).starts_with("2\nbad: hotswap.toml: TOML parse error"));
    ls_only(&["t", "--back"], "2\nt: no previous version");
    let plan = swap_log.trim_end().replace('\n', "\n  ");
    ls_only(&["t", "--check"], &format!("0\nt: none → 2\n  {plan}"));

    // a refused drain: nothing swapped, nothing recorded
    code("t.code");
    assert_eq!(
        run(&["t"]),
        format!("1\nt: t drain refused; nothing swapped\n{ls_log}t drain\n")
    );
    assert!(!state_dir.join("t.json").exists());

    // red with no previous version: stays put, and stays red until a new version or --back
    code("chk.code");
    fs::remove_file(p.join("t.code")).unwrap();
    assert_eq!(run(&["t"]), red("no previous version", ""));
    assert_eq!(state(), r#""2" null false "chk x""#);
    ls_only(&["t"], "1\nt 2: red at chk x");
    ls_only(&["t", "--check"], "0\nt 2: red at chk x");

    fs::remove_file(p.join("chk.code")).unwrap();
    ls("3");
    assert_eq!(
        run(&["t"]),
        format!("0\nt 3: green, 2 smoke probes\n{ls_log}{swap_log}")
    );
    assert_eq!(state(), r#""3" "2" true null"#);
    let smoke = json!([{"code": 0, "output": "version out\nversion err\n", "probe": "t version"}, {"code": 0, "output": "x out\nx err\n", "probe": "chk x"}]);
    assert_eq!(rec("t")["smoke"], smoke);
    ls_only(&["t"], "0\nt 3: up to date");
    // tick: a failed upgrade still swaps every gigantic tool but `tick`, and reds the tick
    let upgrade =
        format!("{ls_log}mise upgrade packslip:jgeschwendt/t packslip:jgeschwendt/t1\n{ls_log}");
    code("mise.code");
    assert_eq!(
        run(&["tick"]),
        format!(
            "1\nupgrade out\nhotswap tick: mise upgrade failed\nt 3: up to date\nt1 1: green, 1 smoke probes\nupgrade err\n{upgrade}{ls_log}chk one\n"
        )
    );
    ls_only(&["t", "--check"], "0\nt 3: up to date");
    assert_eq!(
        run(&["t", "--back"]),
        format!("0\nt 2: green, 2 smoke probes\n{ls_log}{}", back_log("2"))
    );
    assert_eq!(state(), r#""2" "3" true null"#);

    // red with a previous version: roll back under its own manifest, keeping the record's previous
    put("1");
    code("chk.code");
    assert_eq!(run(&["t"]), red("rolled back to 1", &rollback));
    assert_eq!(state(), r#""1" "0" false "chk x""#);
    assert_eq!(probes(), r#""t version" 0, "chk x" 3, "chk one" 0"#);
    // a version that held through a rollback is a rollback target again
    code("chk.code");
    assert_eq!(run(&["t"]), red("rolled back to 1", &rollback));
    // the rollback goes red too
    put("1");
    code("chk.code");
    code("sticky");
    assert_eq!(
        run(&["t"]),
        red("rolled back to 1, where chk one failed too", &rollback)
    );
    assert_eq!(state(), r#""1" "0" false "chk x; after rollback chk one""#);
    assert_eq!(probes(), r#""t version" 0, "chk x" 3, "chk one" 3"#);
    assert_eq!(run(&["t"]), red("1 is red; no rollback", ""));
    // mise use refuses: the failed version stays current
    put("1");
    code("mise.code");
    let refused = "mise use packslip:jgeschwendt/t@1 failed: use err";
    assert_eq!(run(&["t"]), red(refused, &use_log("1")));
    assert_eq!(state(), r#""3" "1" false "chk x""#);
    assert_eq!(
        run(&["t", "--back"]),
        format!("1\nt: {refused}\n{ls_log}{}", use_log("1"))
    );
    assert_eq!(state(), r#""3" "1" false "chk x""#);
    fs::remove_file(p.join("mise.code")).unwrap();
    assert_eq!(
        run(&["t", "--back"]),
        format!("1\nt: chk one failed on 1; no second rollback\n{ls_log}{rollback}")
    );
    assert_eq!(state(), r#""1" "3" false "chk one""#);
    // a crash in the rollback's mise use leaves the failed version's red record; once the pin
    // moved, the next run swaps the pinned version and never rolls back onto the red one
    put("2");
    fs::write(p.join("crash"), "").unwrap();
    assert_eq!(
        run(&["t"]),
        format!("-1\n{ls_log}{swap_log}{}", use_log("2"))
    );
    assert_eq!(state(), r#""3" "2" false "chk x""#);
    ls("2");
    assert_eq!(run(&["t"]), red("3 is red; no rollback", ""));
    assert_eq!(state(), r#""2" "3" false "chk x""#);
    ls("3");
    // the previous version is gone from mise
    put("0");
    assert_eq!(run(&["t"]), red("0 not installed", ""));
    assert_eq!(state(), r#""3" "0" false "chk x""#);
    ls_only(&["t", "--back"], "2\nt 0: not installed");

    // another hotswap holds the lock: refused, but --check reads on
    let held = File::create(state_dir.join("t.lock")).unwrap();
    held.try_lock().unwrap();
    ls_only(&["t"], "2\nt: another hotswap is running");
    ls_only(&["t", "--check"], "0\nt 3: red at chk x");
    drop(held);

    // a probe past its deadline is a red probe and its whole group dies; one whose background child
    // holds the pipe keeps its code
    let late = "sh -c (sleep 2; touch ~/m) & sleep 3";
    ls_only(
        &["slow"],
        &format!("1\nslow: {late} failed; no previous version"),
    );
    assert_eq!(
        rec("slow")["smoke"],
        json!([{"code": 0, "output": "output held open past 1s", "probe": "sh -c sleep 3 & echo up"}, {"code": -1, "output": "timed out after 1s", "probe": late}])
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!p.join("m").exists());

    // a missing command is a red probe; an unwritable lock or record exits 2
    ls_only(&["gone"], "1\ngone: absent failed; no previous version");
    assert_eq!(rec("gone")["smoke"][0]["code"], -1);
    let bare = format!("2\nbare: {}.", state_dir.join("bare").display());
    for f in ["bare.lock", "bare.tmp"] {
        fs::create_dir(state_dir.join(f)).unwrap();
        assert!(run(&["bare"]).starts_with(&bare));
        fs::remove_dir(state_dir.join("bare.lock")).ok();
    }

    // a red swap reds the tick too; a shims dir PATH can't hold refuses; no gigantic tools, no-op
    assert_eq!(
        run(&["tick"]),
        format!(
            "1\nupgrade out\nt 3: red at chk x\nt1 1: up to date\nupgrade err\n{upgrade}{ls_log}"
        )
    );
    assert_eq!(
        go(&["tick"], &[("MISE_DATA_DIR", "a:b")]),
        "2\nhotswap tick: path segment contains separator `:`\n"
    );

    // install: refused over an active version; a failed mise use; then a swap with no drain and no
    // rollback, red, then green over that red record at the same version
    ls_only(&["install", "t"], "1\nt: already installed");
    code("mise.code");
    let new = "mise use -g packslip:jgeschwendt/n\n";
    assert_eq!(
        run(&["install", "n"]),
        format!("1\nn: mise use packslip:jgeschwendt/n failed: use err\n{ls_log}{new}")
    );
    fs::remove_file(p.join("mise.code")).unwrap();
    // `a` is what mise lists once the next mise call ran, `b` until then
    let next = |a: &str, b: &str| {
        ls(a);
        fs::rename(p.join("ls.json"), p.join("next.json")).unwrap();
        ls(b);
    };
    let installed = format!("{ls_log}{new}{ls_log}{}", &swap_log[8..]);
    next("n", "3");
    assert_eq!(
        run(&["install", "n"]),
        format!("1\nn: chk x failed; no previous version\n{installed}")
    );
    fs::remove_file(p.join("sticky")).unwrap();
    next("n", "3");
    assert_eq!(
        run(&["install", "n"]),
        format!("0\nn 1: green, 2 smoke probes\n{installed}")
    );

    // upgrade: mise upgrade --bump, then exactly `hotswap <tool>`
    ls_only(&["upgrade", "t"], "1\nt: not installed");
    ls_only(&["remove", "t"], "1\nt: not installed");
    ls("3");
    code("mise.code");
    let bump = "mise upgrade --bump packslip:jgeschwendt/t\n";
    assert_eq!(
        run(&["upgrade", "t"]),
        format!("1\nt: mise upgrade failed: upgrade err\n{ls_log}{bump}")
    );
    // mise said nothing: no reason, no empty one
    code("quiet");
    assert_eq!(
        run(&["upgrade", "t"]),
        format!("1\nt: mise upgrade failed\n{ls_log}{bump}")
    );
    fs::remove_file(p.join("quiet")).unwrap();
    fs::remove_file(p.join("mise.code")).unwrap();
    put("3");
    assert_eq!(
        run(&["upgrade", "t"]),
        format!("0\nt 3: up to date\n{ls_log}{bump}{ls_log}")
    );
    put("2");
    next("3", "2");
    assert_eq!(
        run(&["upgrade", "t"]),
        format!("0\nt 3: green, 2 smoke probes\n{ls_log}{bump}{ls_log}{swap_log}")
    );
    assert_eq!(state(), r#""3" "2" true null"#);

    // ls: one JSON array — every gigantic tool but the verbs, or the names given in order;
    // mise latest's chatter dropped, its three outcomes, an unknown name
    put("3");
    fs::remove_file(state_dir.join("t1.json")).unwrap();
    let t = r#"{"record":{"at":0,"ok":true,"previous":"0","smoke":[],"version":"3"},"release":"3","tool":"t","version":"3"}"#;
    let t1 = r#"{"reason":"no packslip release yet","record":null,"release":null,"tool":"t1","version":"1"}"#;
    ls_only(&["ls"], &format!("0\n[{t},{t1}]"));
    let lost = r#"{"reason":"no release found","record":null,"release":null,"tool":"lost","version":null}"#;
    let nope = r#"{"reason":"mise latest failed","record":null,"release":null,"tool":"nope","version":null}"#;
    ls_only(
        &["ls", "lost", "nope", "t1"],
        &format!("0\n[{lost},{nope},{t1}]"),
    );
    ls_only(&["ls", "two"], "2\ntwo: several active versions");

    // remove: a failing setup --remove still unuses and drops the record; a failing unuse reds it
    // and keeps the record; a version mise lost has nothing to undo
    let unuse = "t setup --remove\nmise unuse -g packslip:jgeschwendt/t\n";
    code("t.code");
    assert_eq!(
        run(&["remove", "t"]),
        format!("1\nt: t setup --remove failed\n{ls_log}{unuse}")
    );
    assert!(!state_dir.join("t.json").exists());
    fs::remove_file(p.join("t.code")).unwrap();
    code("mise.code");
    put("3");
    assert_eq!(
        run(&["remove", "t"]),
        format!("1\nt: mise unuse packslip:jgeschwendt/t failed: unuse err\n{ls_log}{unuse}")
    );
    assert!(state_dir.join("t.json").exists());
    fs::remove_file(p.join("mise.code")).unwrap();
    assert_eq!(
        run(&["remove", "t"]),
        format!("0\nt: removed\n{ls_log}{unuse}")
    );
    assert!(!state_dir.join("t.json").exists());
    assert_eq!(
        run(&["remove", "lost"]),
        format!("0\nlost: removed\n{ls_log}mise unuse -g lost\n")
    );

    ls("1");
    ls_only(&["tick"], "0\nhotswap tick: no gigantic tools");
}
