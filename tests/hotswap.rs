//! The suite is also the tool: run under a tool's name (argv[0], never a path) it is that tool's
//! `upgrade`, linking hotswap with the manifest named for it below; run bare by cargo, it drives
//! itself through every branch.
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, File},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Command, exit},
};

fn main() {
    let mut args = env::args();
    let tool = args.next().unwrap_or_default();
    let m = json!({
        "gone": {"smoke": [["absent"]]},
        "slow": {"smoke": [["sh", "-c", "sleep 3 & echo up"], ["sh", "-c", "(sleep 2; touch ~/m) & sleep 3"]], "timeout": 1},
        "t": {"agents": ["dev.gigantic.t.upgrade", "dev.gigantic.t"], "drain": [["t", "drain"]], "setup": ["t", "setup"], "smoke": [["t", "version"], ["chk", "x"]]},
    });
    match tool.as_str() {
        "forget" => hotswap::forget("t"),
        "plist" => print!("{}", hotswap::agent_plist("t")),
        t if !t.contains('/') => {
            let m = serde_json::from_value(m.get(t).cloned().unwrap_or(json!({}))).unwrap();
            exit(hotswap::upgrade(t, &m, &args.collect::<Vec<_>>()))
        }
        _ => suite(),
    }
}

// One host walked through every branch: `mise`, `launchctl` and the tool's commands are one sh stub
// that logs its argv; `<name>.code` or `<name>.<verb>.code` makes it exit non-zero, and `mise use`
// clears those unless `sticky` exists, `<verb>.out` replaces its stdout, `quiet` silences its reason (mise's chatter stays), and a
// one-shot `crash` SIGKILLs its caller. `mise ls` answers only from `/`; any other mise call moves
// `next.json` over `ls.json`. `old/t` leads PATH, as a shim leaves the running install's bin dir: a
// step that runs it fails the exact log match. The temp dir's `&<>` must be escaped in the plist.
fn suite() {
    let d = tempfile::Builder::new().prefix("hot&<>").tempdir().unwrap();
    let p = d.path();
    let (bin, old) = (p.join("bin"), p.join("old"));
    let shims = p.join(".local/share/mise/shims");
    let stub = format!(
        "#!/bin/sh\nn=${{0##*/}}\necho \"$n $*\" >> '{0}/log'\n[ \"$n $1\" = 'mise ls' ] && [ \"$(pwd -P)\" = / ] && cd '{0}' && exec cat ls.json\n[ \"$n\" = mise ] && mv '{0}/next.json' '{0}/ls.json' 2>/dev/null\n[ \"$n $1\" = 'mise use' ] && rm '{0}/crash' 2>/dev/null && kill -9 $PPID\n[ \"$n $1\" = 'mise use' ] && [ ! -e '{0}/sticky' ] && rm -f '{0}'/*.code\ncat '{0}'/\"$1.out\" 2>/dev/null || echo \"$1 out\"; [ -e '{0}/quiet' ] || echo \"$1 err\" >&2\n[ \"$n\" = mise ] && echo ' WARN  x' >&2 && echo 'run with --verbose' >&2\nexit $(cat \"{0}/$n.code\" 2>/dev/null || cat \"{0}/$n.$1.code\" 2>/dev/null || echo 0)\n",
        p.display()
    );
    for (dir, name, body) in [
        (&bin, "chk", &*stub),
        (&bin, "launchctl", &stub),
        (&bin, "mise", &stub),
        (&old, "mise", "not executable, so the plist looks further"),
        (
            &old,
            "t",
            &format!("#!/bin/sh\necho \"old t $*\" >> '{}/log'\n", p.display()),
        ),
        (&shims, "t", &stub),
    ] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
        let mode = if name == "mise" && dir == &old {
            0o644
        } else {
            0o755
        };
        fs::set_permissions(dir.join(name), fs::Permissions::from_mode(mode)).unwrap();
    }
    // `v` is the active one of `t`'s 1, 2 and 3
    let ls = |v: &str| {
        let e = |active: bool, version: &str| json!({"active": active, "installed": true, "version": version});
        let one = |active| json!([e(active, "1")]);
        let t = json!([e(false, "1"), e(v == "2", "2"), e(v == "3", "3")]);
        let all = json!({"bare": one(true), "cargo:gone": one(true), "idle": one(false), "packslip:jgeschwendt/t": t, "slow": one(true), "two": [e(true, "1"), e(true, "2")]});
        fs::write(p.join("ls.json"), all.to_string()).unwrap();
    };
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    // exit code, stdout + stderr, then the stubs' log
    let go = |tool: &str, args: &[&str], envs: &[(&str, &str)]| {
        let path = format!("{}:{}:/bin:/usr/bin", old.display(), bin.display());
        let o = Command::new(env::current_exe().unwrap())
            .arg0(tool)
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
    // a green record at `v`, whose previous is 0 — a version mise no longer holds
    let put = |v: &str| {
        let r = json!({"at": 0, "ok": true, "previous": "0", "smoke": [], "version": v});
        fs::write(state_dir.join("t.json"), r.to_string()).unwrap();
    };
    let code = |name: &str| fs::write(p.join(name), "3").unwrap();
    let rm = |name: &str| fs::remove_file(p.join(name)).unwrap();
    let ls_log = "mise ls --json\n";
    // a run that stops after the `mise ls` lookup, printing `want`
    let ls_only = |args: &[&str], want: &str| assert_eq!(run(args), format!("{want}\n{ls_log}"));
    let bump = |key: &str| format!("mise upgrade --bump {key}\n");
    let latest = |key: &str| format!("mise latest {key}\n");
    let up = |key: &str| format!("{ls_log}{}{}{ls_log}", latest(key), bump(key));
    let t_up = up("packslip:jgeschwendt/t");
    let t_latest = latest("packslip:jgeschwendt/t");
    // --check stops after the lookups, printing `want`
    let check = |want: &str| assert_eq!(run(&["--check"]), format!("{want}\n{ls_log}{t_latest}"));
    let swap_log =
        "t drain\nt setup\nlaunchctl kickstart -k gui/$UID/dev.gigantic.t\nt version\nchk x\n";
    let use_log = |v: &str| format!("mise use -g packslip:jgeschwendt/t@{v}\n");
    let back_log = |v: &str| format!("{}{}", use_log(v), &swap_log[8..]);
    let rollback = back_log("1");
    let red = |why: &str, rest: &str| format!("1\nt: chk x failed; {why}\n{t_up}{swap_log}{rest}");

    let usage = "2\nusage: t upgrade [--check | --back | --retry]\n";
    assert_eq!(run(&["--x"]), usage);
    assert_eq!(run(&["--back", "--check"]), usage);
    assert_eq!(go("t", &[], &[("HOME", "")]), "2\nHOME unset\n");

    // the plist: shims from MISE_DATA_DIR, XDG_DATA_HOME, then HOME; mise's dir from PATH, else
    // mise's installer default
    let home = p.display().to_string();
    let plist = |shims: &str, mise: &str| {
        format!(
            r#"0
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.gigantic.t.upgrade</string>
  <key>ProgramArguments</key>
  <array>
    <string>{shims}/t</string>
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
  <key>StandardOutPath</key><string>{home}/Library/Logs/dev.gigantic.t.upgrade.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/dev.gigantic.t.upgrade.log</string>
</dict>
</plist>
"#
        )
        .replace("hot&<>", "hot&amp;&lt;&gt;")
    };
    let bins = bin.display().to_string();
    for (envs, shims) in [
        (
            &[("MISE_DATA_DIR", "/m"), ("XDG_DATA_HOME", "/x")][..],
            "/m/shims".into(),
        ),
        (&[("XDG_DATA_HOME", "/x")], "/x/mise/shims".into()),
        (&[], format!("{home}/.local/share/mise/shims")),
    ] {
        assert_eq!(go("plist", &[], envs), plist(&shims, &bins));
    }

    // mise missing, failing, then talking nonsense
    ls_only(&[], "2\nmise: cat: ls.json: No such file or directory");
    fs::write(p.join("ls.json"), "x").unwrap();
    ls_only(&[], "2\nmise: expected value at line 1 column 1");
    fs::rename(bin.join("mise"), p.join("mise")).unwrap();
    assert_eq!(run(&[]), "2\nmise: Permission denied (os error 13)\n");
    let fallback = plist(
        &format!("{home}/.local/share/mise/shims"),
        &format!("{home}/.local/bin"),
    );
    assert_eq!(go("plist", &[], &[]), fallback);
    fs::rename(p.join("mise"), bin.join("mise")).unwrap();
    ls("2");

    for msg in [
        "idle: no active mise install",
        "nope: no active mise install",
        "two: several active versions",
    ] {
        let tool = msg.split(':').next().unwrap();
        assert_eq!(go(tool, &[], &[]), format!("2\n{msg}\n{ls_log}"));
    }
    ls_only(&["--back"], "2\nt: no previous version");
    let plan = swap_log.trim_end().replace('\n', "\n  ");
    check(&format!("0\nt: none → 2\n  {plan}"));

    // mise upgrade --bump fails, with its reason past its chatter, then with none: nothing swapped
    code("mise.upgrade.code");
    let t_bump = bump("packslip:jgeschwendt/t");
    let failed = |why: &str| format!("1\nt: mise upgrade failed{why}\n{ls_log}{t_latest}{t_bump}");
    assert_eq!(run(&[]), failed(": upgrade err"));
    code("quiet");
    assert_eq!(run(&[]), failed(""));
    rm("quiet");
    rm("mise.upgrade.code");

    // a refused drain: nothing swapped, nothing recorded
    code("t.code");
    assert_eq!(
        run(&[]),
        format!("1\nt: t drain refused; nothing swapped\n{t_up}t drain\n")
    );
    assert!(!state_dir.join("t.json").exists());

    // red with no previous version: stays put, and stays red until a new version or --back
    code("chk.code");
    rm("t.code");
    assert_eq!(run(&[]), red("no previous version", ""));
    assert_eq!(state(), r#""2" null false "chk x" "2""#);
    assert_eq!(run(&[]), format!("1\nt 2: red at chk x\n{t_up}"));
    check("0\nt 2: red at chk x");

    // the bump moves mise to 3: green
    rm("chk.code");
    put("2");
    let next = |a: &str, b: &str| {
        ls(a);
        fs::rename(p.join("ls.json"), p.join("next.json")).unwrap();
        ls(b);
    };
    next("3", "2");
    assert_eq!(
        run(&[]),
        format!("0\nt 3: green, 2 smoke probes\n{t_up}{swap_log}")
    );
    assert_eq!(state(), r#""3" "2" true null null"#);
    let smoke = json!([{"code": 0, "output": "version out\nversion err\n", "probe": "t version"}, {"code": 0, "output": "x out\nx err\n", "probe": "chk x"}]);
    assert_eq!(rec("t")["smoke"], smoke);
    assert_eq!(run(&[]), format!("0\nt 3: up to date\n{t_up}"));
    check("0\nt 3: up to date");
    assert_eq!(
        run(&["--back"]),
        format!("0\nt 2: green, 2 smoke probes\n{ls_log}{}", back_log("2"))
    );
    assert_eq!(state(), r#""2" "3" true null "3""#);

    // red with a previous version: roll back under the same manifest, keeping the record's previous
    put("1");
    code("chk.code");
    assert_eq!(run(&[]), red("rolled back to 1", &rollback));
    assert_eq!(state(), r#""1" "0" false "chk x" "3""#);
    let back = r#""t version" 0, "chk x" 3, "t version" 0"#;
    assert_eq!(probes(), format!(r#"{back}, "chk x" 0"#));
    // the version rolled back from is held while it is latest; --retry takes it again, and a
    // version that held through a rollback is a rollback target again
    fs::write(p.join("latest.out"), "3\n").unwrap();
    let wait = "t: 3 is red; waiting for a newer release";
    assert_eq!(run(&[]), format!("1\n{wait}\n{ls_log}{t_latest}"));
    check(&format!("0\n{wait}"));
    code("chk.code");
    assert_eq!(
        run(&["--retry"]),
        format!(
            "1\nt: chk x failed; rolled back to 1\n{ls_log}{t_bump}{ls_log}{swap_log}{rollback}"
        )
    );
    // a failed `mise latest` goes through, and so does a newer latest
    for f in ["mise.latest.code", "latest.out"] {
        fs::write(p.join(f), "4").unwrap();
        code("chk.code");
        assert_eq!(run(&[]), red("rolled back to 1", &rollback));
    }
    rm("latest.out");
    // the rollback goes red too
    put("1");
    code("chk.code");
    code("sticky");
    assert_eq!(
        run(&[]),
        red("rolled back to 1, where chk x failed too", &rollback)
    );
    assert_eq!(
        state(),
        r#""1" "0" false "chk x; after rollback chk x" "3""#
    );
    assert_eq!(probes(), format!(r#"{back}, "chk x" 3"#));
    assert_eq!(run(&[]), red("1 is red; no rollback", ""));
    // mise use refuses: the failed version stays current
    put("1");
    code("mise.use.code");
    let refused = "mise use packslip:jgeschwendt/t@1 failed: use err";
    assert_eq!(run(&[]), red(refused, &use_log("1")));
    assert_eq!(state(), r#""3" "1" false "chk x" "3""#);
    assert_eq!(
        run(&["--back"]),
        format!("1\nt: {refused}\n{ls_log}{}", use_log("1"))
    );
    assert_eq!(state(), r#""3" "1" false "chk x" "3""#);
    rm("mise.use.code");
    assert_eq!(
        run(&["--back"]),
        format!("1\nt: chk x failed on 1; no second rollback\n{ls_log}{rollback}")
    );
    assert_eq!(state(), r#""1" "3" false "chk x" "3""#);
    // a crash in the rollback's mise use leaves the failed version's red record; once the pin
    // moved, the next run swaps the pinned version and never rolls back onto the red one
    put("2");
    fs::write(p.join("crash"), "").unwrap();
    assert_eq!(run(&[]), format!("-1\n{t_up}{swap_log}{}", use_log("2")));
    assert_eq!(state(), r#""3" "2" false "chk x" "3""#);
    ls("2");
    assert_eq!(run(&[]), red("3 is red; no rollback", ""));
    assert_eq!(state(), r#""2" "3" false "chk x" "2""#);
    ls("3");
    // the previous version is gone from mise
    put("0");
    assert_eq!(run(&[]), red("0 not installed", ""));
    assert_eq!(state(), r#""3" "0" false "chk x" "3""#);
    ls_only(&["--back"], "2\nt 0: not installed");

    // another upgrade holds the lock: refused, but --check reads on
    let held = File::create(state_dir.join("t.lock")).unwrap();
    held.try_lock().unwrap();
    assert_eq!(
        run(&[]),
        format!("2\nt: another hotswap is running\n{ls_log}")
    );
    check("0\nt 3: red at chk x");
    drop(held);

    // a probe past its deadline is a red probe and its whole group dies; one whose background child
    // holds the pipe keeps its code
    let late = "sh -c (sleep 2; touch ~/m) & sleep 3";
    assert_eq!(
        go("slow", &[], &[]),
        format!(
            "1\nslow: {late} failed; no previous version\n{}",
            up("slow")
        )
    );
    assert_eq!(
        rec("slow")["smoke"],
        json!([{"code": 0, "output": "output held open past 1s", "probe": "sh -c sleep 3 & echo up"}, {"code": -1, "output": "timed out after 1s", "probe": late}])
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!p.join("m").exists());

    // a missing command is a red probe; an unwritable lock or record exits 2
    assert_eq!(
        go("gone", &[], &[]),
        format!(
            "1\ngone: absent failed; no previous version\n{}",
            up("cargo:gone")
        )
    );
    assert_eq!(rec("gone")["smoke"][0]["code"], -1);
    let bare = format!("2\nbare: {}.", state_dir.join("bare").display());
    for f in ["bare.lock", "bare.tmp"] {
        fs::create_dir(state_dir.join(f)).unwrap();
        assert!(go("bare", &[], &[]).starts_with(&bare));
        fs::remove_dir(state_dir.join("bare.lock")).ok();
    }

    // forget drops the record and the lock, absent or not
    for _ in 0..2 {
        assert_eq!(go("forget", &[], &[]), "0\n");
    }
    assert!(!state_dir.join("t.json").exists() && !state_dir.join("t.lock").exists());
}
