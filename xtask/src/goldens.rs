//! `cargo xtask goldens [--record]` (spec: replay-and-evaluation, Golden
//! recording regression gate).
//!
//! Without `--record` it replays every `tests/golden/*.pmtrec` in `strict`
//! mode, in-process, and fails naming each recording that diverges or
//! whose trajectory no longer matches. `--record` re-creates them from
//! `tests/golden/sources/<name>/`: `scenario.toml` (`prompts`, the later
//! ones resuming the session; `set` overrides; `lineage`), `mock.toml`,
//! an optional `config.toml` and the `workspace/` the session runs in,
//! each run by the built binary in a scratch home under `target/`, then
//! exported with `prometheus replay export --include-workspace`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pmt_core::env::{EnvPolicy, LaunchEnv};
use serde::Deserialize;

/// Where the goldens live.
const DIR: &str = "tests/golden";

/// One scenario's `scenario.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    prompts: Vec<String>,
    #[serde(default)]
    set: Vec<String>,
    #[serde(default)]
    lineage: bool,
}

/// Runs the gate, or re-records with `record`.
pub(crate) fn run(root: &Path, record: bool) -> Result<(), String> {
    if record {
        return record_all(root);
    }
    let launch = LaunchEnv::from_process(&EnvPolicy::default()).map_err(|e| e.to_string())?;
    let scratch = root.join("target/goldens");
    let dir = root.join(DIR);
    let (failures, ran) = prometheus::replay::gate::check(&dir, &scratch, &launch)
        .map_err(|e| format!("{DIR}: {e}"))?;
    if ran == 0 {
        return Err(format!("{DIR} holds no golden recordings"));
    }
    for failure in &failures {
        eprintln!(
            "golden recording diverged: {}\n{}",
            failure.name, failure.report
        );
    }
    if failures.is_empty() {
        println!("goldens: {ran} recordings replayed without divergence");
        return Ok(());
    }
    let names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
    Err(format!(
        "{} golden recording(s) diverged: {} (an intended change is accepted with `prometheus replay bless <file>` or `cargo xtask goldens --record`)",
        names.len(),
        names.join(", ")
    ))
}

/// The debug binary, built now.
pub(crate) fn binary(root: &Path) -> Result<PathBuf, String> {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "prometheus", "--all-features", "--locked"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build -p prometheus failed".to_owned());
    }
    let name = format!("prometheus{}", std::env::consts::EXE_SUFFIX);
    Ok(root.join("target/debug").join(name))
}

fn record_all(root: &Path) -> Result<(), String> {
    let bin = binary(root)?;
    let sources = root.join(DIR).join("sources");
    let mut names: Vec<String> = std::fs::read_dir(&sources)
        .map_err(|e| format!("{}: {e}", sources.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let file = root.join(DIR).join(format!("{name}.pmtrec"));
        record(
            &bin,
            &sources.join(&name),
            &root.join("target/goldens-record").join(&name),
            &file,
        )?;
        println!("recorded {}", file.display());
    }
    Ok(())
}

/// Runs one scenario and exports its session to `file`.
fn record(bin: &Path, source: &Path, scratch: &Path, file: &Path) -> Result<(), String> {
    let read = |name: &str| std::fs::read_to_string(source.join(name));
    let scenario: Scenario = toml::from_str(&read("scenario.toml").map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", source.display()))?;
    let _ = std::fs::remove_dir_all(scratch);
    let home = scratch.join("home");
    let work = scratch.join("work");
    let profile = home.join("profiles/default");
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    copy_dir(&source.join("workspace"), &work)?;
    std::fs::write(
        profile.join("config.toml"),
        read("config.toml").unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let mock = source.join("mock.toml");
    let mut session: Option<String> = None;
    for prompt in &scenario.prompts {
        let mut cmd = prometheus(bin, &home);
        for set in &scenario.set {
            cmd.args(["--set", set]);
        }
        cmd.args(["chat", "-z", prompt, "--provider", "mock", "--mock-script"])
            .arg(&mock)
            .current_dir(&work);
        if let Some(id) = &session {
            cmd.args(["-r", id]);
        }
        run_quietly(&mut cmd)?;
        session.get_or_insert(newest_session(bin, &home)?);
    }
    let id = session.ok_or("a scenario needs at least one prompt")?;
    let mut export = prometheus(bin, &home);
    export
        .args(["replay", "export", &id, "--include-workspace", "-o"])
        .arg(file);
    if scenario.lineage {
        export.arg("--lineage");
    }
    run_quietly(&mut export)
}

/// The binary on `home`, with no inherited Prometheus or provider
/// variables.
fn prometheus(bin: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(bin);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    cmd.arg("--home").arg(home).stdin(Stdio::null());
    cmd
}

/// Runs `cmd`; a chat that ends with a budget or another turn failure is
/// still a recording, so only a failure to record at all is an error.
fn run_quietly(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    let code = out.status.code().unwrap_or(-1);
    // 0 done, 1 failed turn, 3 budget, 4 approval needed: all recorded.
    if [0, 1, 3, 4].contains(&code) {
        return Ok(());
    }
    Err(format!(
        "{cmd:?} exited {code}: {}",
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn newest_session(bin: &Path, home: &Path) -> Result<String, String> {
    let out = prometheus(bin, home)
        .args(["--json", "sessions", "list", "--all"])
        .output()
        .map_err(|e| e.to_string())?;
    let listed: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    listed["sessions"][0]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "no session was recorded".to_owned())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries.filter_map(Result::ok) {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
