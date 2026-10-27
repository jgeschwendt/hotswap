use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

// One host walked through every branch: `mise`, `launchctl` and the tool's commands are one sh stub
// that logs its argv; `<name>.code` makes that stub exit non-zero.
#[test]
fn hotswap() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let bin = p.join("bin");
    for (name, body) in [
        ("bad/hotswap.toml", "agents = 1"),
        ("gone/hotswap.toml", r#"smoke = [["absent"]]"#),
        (
            "t/hotswap.toml",
            r#"agents = ["dev.gigantic.t"]
drain = [["t", "drain"]]
setup = ["t", "setup"]
smoke = [["t", "version"], ["chk", "x"]]"#,
        ),
    ] {
        fs::create_dir_all(p.join(name).parent().unwrap()).unwrap();
        fs::write(p.join(name), body).unwrap();
    }
    fs::create_dir_all(&bin).unwrap();
    for name in ["chk", "launchctl", "mise", "t"] {
        let stub = format!(
            "#!/bin/sh\nn=${{0##*/}}; echo \"$n $*\" >> '{0}/log'\n[ \"$n $1\" = 'mise ls' ] && exec cat '{0}/ls.json'\necho \"$1 out\"; echo \"$1 err\" >&2\nexit $(cat \"{0}/$n.code\" 2>/dev/null || echo 0)\n",
            p.display()
        );
        fs::write(bin.join(name), stub).unwrap();
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ls = |v: &str| {
        let at = |dir: &str, active: bool| json!([{"active": active, "install_path": p.join(dir), "version": "1"}]);
        let t = json!([{"active": false, "install_path": p.join("t"), "version": "1"}, {"active": true, "install_path": p.join("t"), "version": v}]);
        let all = json!({"aqua:x/bad": at("bad", true), "bare": at("bare", true), "cargo:gone": at("gone", true), "idle": at("t", false), "packslip:jgeschwendt/t": t});
        fs::write(p.join("ls.json"), all.to_string()).unwrap();
    };
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    // exit code, stdout + stderr, then the stubs' log
    let run = |args: &[&str]| {
        let path = format!("{}:/bin:/usr/bin", bin.display());
        let o = Command::new(env!("CARGO_BIN_EXE_hotswap"))
            .args(args)
            .env("HOME", p)
            .env("PATH", path)
            .output()
            .unwrap();
        let log = fs::read_to_string(p.join("log")).unwrap_or_default();
        fs::remove_file(p.join("log")).ok();
        let out = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
        format!("{}\n{out}{log}", o.status.code().unwrap())
            .replace(&format!("/{}/", uid.trim()), "/$UID/")
    };
    let rec = |tool: &str| {
        serde_json::from_slice::<Value>(
            &fs::read(p.join(format!(".local/state/hotswap/{tool}.json"))).unwrap(),
        )
        .unwrap()
    };
    let state = || {
        let r = rec("t");
        format!(
            "{} {} {} {}",
            r["version"], r["previous"], r["ok"], r["failed"]
        )
    };
    let usage = "2\nusage: hotswap <tool> [--check | --back]\n";
    let ls_log = "mise ls --global --json\n";
    let swap_log =
        "t drain\nlaunchctl kickstart -k gui/$UID/dev.gigantic.t\nt setup\nt version\nchk x\n";
    let back_log = |v: &str| format!("mise use -g packslip:jgeschwendt/t@{v}\n{}", &swap_log[8..]);
    ls("2");

    assert_eq!(run(&[]), usage);
    assert_eq!(run(&["--back"]), usage);
    assert_eq!(run(&["t", "--x"]), usage);
    assert_eq!(
        run(&["nope"]),
        format!("2\nnope: no active mise install\n{ls_log}")
    );
    assert_eq!(
        run(&["idle"]),
        format!("2\nidle: no active mise install\n{ls_log}")
    );
    assert!(run(&["bad"]).starts_with("2\nbad: hotswap.toml: TOML parse error"));
    assert_eq!(
        run(&["t", "--back"]),
        format!("2\nt: no previous version\n{ls_log}")
    );
    let plan = swap_log.trim_end().replace('\n', "\n  ");
    assert_eq!(
        run(&["t", "--check"]),
        format!("0\nt: none → 2\n  {plan}\n{ls_log}")
    );

    // red with no previous version: stays put, record says why
    fs::write(p.join("chk.code"), "3").unwrap();
    assert_eq!(
        run(&["t"]),
        format!("1\nt: chk x failed; no previous version\n{ls_log}{swap_log}")
    );
    assert_eq!(state(), r#""2" null false "chk x""#);

    fs::remove_file(p.join("chk.code")).unwrap();
    assert_eq!(
        run(&["t"]),
        format!("0\nt 2: green, 2 smoke probes\n{ls_log}{swap_log}")
    );
    assert_eq!(state(), r#""2" null true null"#);
    let smoke = json!([{"code": 0, "output": "version out\nversion err\n", "probe": "t version"}, {"code": 0, "output": "x out\nx err\n", "probe": "chk x"}]);
    assert_eq!(rec("t")["smoke"], smoke);
    assert_eq!(run(&["t"]), format!("0\nt 2: up to date\n{ls_log}"));
    assert_eq!(
        run(&["t", "--check"]),
        format!("0\nt 2: up to date\n{ls_log}")
    );

    ls("3");
    assert_eq!(
        run(&["t"]),
        format!("0\nt 3: green, 2 smoke probes\n{ls_log}{swap_log}")
    );
    assert_eq!(state(), r#""3" "2" true null"#);
    assert_eq!(
        run(&["t", "--back"]),
        format!("0\nt 2: green, 2 smoke probes\n{ls_log}{}", back_log("2"))
    );
    assert_eq!(state(), r#""2" "3" true null"#);

    // red with a previous version: roll back, swap again, record the failed run
    fs::write(p.join("chk.code"), "3").unwrap();
    let rolled = format!(
        "1\nt: chk x failed; rolled back to 2\n{ls_log}{swap_log}{}",
        back_log("2")
    );
    assert_eq!(run(&["t"]), rolled);
    assert_eq!(state(), r#""2" "3" false "chk x""#);
    assert_eq!(rec("t")["smoke"][1]["code"], 3);
    assert_eq!(
        run(&["t", "--back"]),
        format!(
            "1\nt: chk x failed on 3; no second rollback\n{ls_log}{}",
            back_log("3")
        )
    );

    // no manifest: nothing to run; a missing command is a red probe; an unwritable record exits 2
    assert_eq!(
        run(&["bare"]),
        format!("0\nbare 1: green, 0 smoke probes\n{ls_log}")
    );
    assert_eq!(
        run(&["gone"]),
        format!("1\ngone: absent failed; no previous version\n{ls_log}")
    );
    assert_eq!(rec("gone")["smoke"][0]["code"], -1);
    fs::create_dir(p.join(".local/state/hotswap/gone.tmp")).unwrap();
    assert!(run(&["gone"]).starts_with("2\ngone: "));
}
