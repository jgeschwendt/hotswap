//! hotswap — the swap moment for a mise-installed tool: drain, swap, smoke, then settle or roll back.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fmt::Display,
    fs, mem,
    path::PathBuf,
    process::{Command, exit},
    time::SystemTime,
};

#[derive(Deserialize)]
struct Install {
    active: bool,
    install_path: PathBuf,
    version: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Manifest {
    agents: Vec<String>,
    drain: Vec<Vec<String>>,
    setup: Vec<String>,
    smoke: Vec<Vec<String>>,
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
    let args: Vec<String> = env::args().skip(1).collect();
    let (tool, mode) = match &args[..] {
        [t] if !t.starts_with('-') => (t, ""),
        [t, m] if !t.starts_with('-') && (m == "--back" || m == "--check") => (t, m.as_str()),
        _ => die("usage: hotswap <tool> [--check | --back]"),
    };
    let ls = Command::new("mise")
        .args(["ls", "--global", "--json"])
        .output();
    let tools: BTreeMap<String, Vec<Install>> = ls
        .ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or_default();
    let Some((key, cur)) = tools
        .into_iter()
        .find(|(k, _)| k.rsplit([':', '/']).next() == Some(tool))
        .and_then(|(k, v)| Some((k, v.into_iter().find(|i| i.active)?)))
    else {
        die(format!("{tool}: no active mise install"))
    };
    let manifest = fs::read_to_string(cur.install_path.join("hotswap.toml")).unwrap_or_default();
    let m: Manifest =
        toml::from_str(&manifest).unwrap_or_else(|e| die(format!("{tool}: hotswap.toml: {e}")));
    let dir = PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/state/hotswap");
    let path = dir.join(format!("{tool}.json"));
    let old: Option<Record> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let uid = uid.unwrap_or_default();
    let mut tail: Vec<Vec<String>> = m
        .agents
        .iter()
        .map(|a| {
            ["launchctl", "kickstart", "-k", &format!("gui/{uid}/{a}")]
                .map(String::from)
                .into()
        })
        .collect();
    tail.extend((!m.setup.is_empty()).then_some(m.setup));
    if mode != "--back"
        && old
            .as_ref()
            .is_some_and(|r| r.ok && r.version == cur.version)
    {
        return println!("{tool} {}: up to date", cur.version);
    }
    if mode == "--check" {
        println!(
            "{tool}: {} → {}",
            old.map_or("none".into(), |r| r.version),
            cur.version
        );
        for argv in m.drain.iter().chain(&tail).chain(&m.smoke) {
            println!("  {}", argv.join(" "));
        }
        return;
    }
    // Runs `pre`, then the kickstarts and setup, then the smoke probes; stops at the first failure.
    let swap = |pre: &[Vec<String>]| {
        let mut smoke = vec![];
        for (i, argv) in pre.iter().chain(&tail).chain(&m.smoke).enumerate() {
            let run = Command::new(argv.first().unwrap_or(&String::new()))
                .args(argv.iter().skip(1))
                .output();
            let (code, output) = run.map_or_else(
                |e| (-1, e.to_string()),
                |o| {
                    (
                        o.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into(),
                    )
                },
            );
            let probe = argv.join(" ");
            if i >= pre.len() + tail.len() {
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
    let back = mode == "--back";
    let mise_use = |v: &str| {
        vec![
            ["mise", "use", "-g", &format!("{key}@{v}")]
                .map(String::from)
                .to_vec(),
        ]
    };
    let (mut version, mut previous, pre) = if back {
        let p = old
            .and_then(|r| r.previous)
            .unwrap_or_else(|| die(format!("{tool}: no previous version")));
        (p.clone(), Some(cur.version), mise_use(&p))
    } else {
        let p = old.map(|r| r.version).filter(|v| *v != cur.version);
        (cur.version, p, m.drain)
    };
    let (failed, smoke) = swap(&pre);
    let line = match &failed {
        None => format!("{tool} {version}: green, {} smoke probes", smoke.len()),
        Some(f) if back => format!("{tool}: {f} failed on {version}; no second rollback"),
        Some(f) => match previous.take() {
            None => format!("{tool}: {f} failed; no previous version"),
            Some(p) => {
                let (again, _) = swap(&mise_use(&p));
                previous = Some(mem::replace(&mut version, p));
                let again = again.map_or(String::new(), |a| format!(", where {a} failed too"));
                format!("{tool}: {f} failed; rolled back to {version}{again}")
            }
        },
    };
    let code = i32::from(failed.is_some());
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
    fs::create_dir_all(&dir)
        .and_then(|()| fs::write(&tmp, serde_json::to_string(&record)?))
        .and_then(|()| fs::rename(&tmp, &path))
        .unwrap_or_else(|e| die(format!("{tool}: {}: {e}", path.display())));
    println!("{line}");
    exit(code)
}
