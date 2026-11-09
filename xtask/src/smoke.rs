//! `cargo xtask smoke` (spec: replay-and-evaluation, Built-in smoke
//! suite): every task of the `smoke` suite bundled in the binary, run by
//! the built binary on the mock provider in a scratch home under
//! `target/smoke/`, and graded exactly; any task below its grade fails
//! the check. A task needing a harness capability this platform lacks is
//! skipped and listed.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use prometheus::smoke_suite;
use serde::Deserialize;

/// `task.toml` (the parts the smoke run reads).
#[derive(Debug, Deserialize)]
struct Task {
    prompt: String,
    #[serde(default)]
    files: Vec<String>,
}

/// `run.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    #[serde(default)]
    set: Vec<String>,
    #[serde(default)]
    then: Vec<String>,
    #[serde(default)]
    config: String,
    #[serde(default)]
    interrupt_after_turn_start: bool,
    requires: Option<String>,
}

/// `grader/grader.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grader {
    #[serde(rename = "type")]
    kind: String,
    of: String,
    expect: String,
}

/// Harness capabilities this platform has.
fn has(capability: &str) -> bool {
    capability == "posix-signals" && cfg!(unix)
}

pub(crate) fn run(root: &Path) -> Result<(), String> {
    let bin = crate::goldens::binary(root)?;
    let ids = smoke_suite::task_ids();
    let (mut passed, mut failed, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    for id in &ids {
        let parse = |name: &str| smoke_suite::file(&format!("tasks/{id}/{name}"));
        let run: Run = parse("run.toml")
            .map(toml::from_str)
            .transpose()
            .map_err(|e| format!("{id}/run.toml: {e}"))?
            .unwrap_or_default();
        if let Some(needed) = run.requires.as_deref().filter(|c| !has(c)) {
            skipped.push(format!("{id} (needs {needed})"));
            continue;
        }
        let scratch = root.join("target/smoke").join(id);
        match grade(&bin, id, &run, &scratch)? {
            Ok(()) => passed.push(*id),
            Err(why) => failed.push(format!("{id}: {why}")),
        }
    }
    println!(
        "smoke: {} of {} tasks passed",
        passed.len(),
        ids.len() - skipped.len()
    );
    for task in &skipped {
        println!("skipped: {task}");
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(format!("smoke tasks failed:\n  {}", failed.join("\n  ")))
}

/// Runs task `id`; `Ok(Err(why))` when it ran and missed its grade.
fn grade(bin: &Path, id: &str, run: &Run, scratch: &Path) -> Result<Result<(), String>, String> {
    let read = |name: &str| {
        smoke_suite::file(&format!("tasks/{id}/{name}")).ok_or_else(|| format!("{id}: no {name}"))
    };
    let task: Task = toml::from_str(read("task.toml")?).map_err(|e| format!("{id}: {e}"))?;
    let grader: Grader =
        toml::from_str(read("grader/grader.toml")?).map_err(|e| format!("{id}: {e}"))?;
    if grader.kind != "exact" {
        return Err(format!("{id}: unsupported grader type {}", grader.kind));
    }
    let _ = std::fs::remove_dir_all(scratch);
    let (home, work) = (scratch.join("home"), scratch.join("work"));
    let profile = home.join("profiles/default");
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    std::fs::write(profile.join("config.toml"), &run.config).map_err(|e| e.to_string())?;
    for name in &task.files {
        std::fs::write(work.join(name), read(&format!("files/{name}"))?)
            .map_err(|e| e.to_string())?;
    }
    let mock = scratch.join("mock.toml");
    std::fs::write(&mock, read("mock.toml")?).map_err(|e| e.to_string())?;
    let mut out = chat(bin, (&home, &work, &mock), run, &task.prompt, None)?;
    for prompt in &run.then {
        let session = newest_session(bin, &home)?;
        out = chat(bin, (&home, &work, &mock), run, prompt, Some(&session))?;
    }
    let actual = match grader.of.as_str() {
        "answer" => String::from_utf8_lossy(&out.stdout).into_owned(),
        "exit" => out.status.code().map(|c| c.to_string()).unwrap_or_default(),
        file => match file.strip_prefix("file:") {
            Some(path) => std::fs::read_to_string(work.join(path)).unwrap_or_default(),
            None => return Err(format!("{id}: unknown grader target {file}")),
        },
    };
    // Normalised string equality: surrounding whitespace aside.
    if actual.trim() == grader.expect.trim() {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "expected {:?}, got {:?} (stderr: {})",
        grader.expect,
        actual.trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

fn base(bin: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(bin);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    cmd.arg("--home").arg(home);
    cmd
}

/// One one-shot chat of the task.
fn chat(
    bin: &Path,
    (home, work, mock): (&Path, &Path, &Path),
    run: &Run,
    prompt: &str,
    resume: Option<&str>,
) -> Result<Output, String> {
    let mut cmd = base(bin, home);
    for set in &run.set {
        cmd.args(["--set", set]);
    }
    cmd.args(["chat", "-z", prompt, "--provider", "mock", "--mock-script"])
        .arg(mock)
        .current_dir(work)
        .stdin(Stdio::null());
    if let Some(id) = resume {
        cmd.args(["-r", id]);
    }
    if !run.interrupt_after_turn_start {
        return cmd.output().map_err(|e| e.to_string());
    }
    interrupted(cmd, home)
}

/// Starts `cmd`, sends it SIGINT once its turn has started, and waits.
#[cfg(unix)]
fn interrupted(mut cmd: Command, home: &Path) -> Result<Output, String> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let trajectories = home.join("profiles/default/trajectories");
    while !turn_started(&trajectories) {
        if started.elapsed() > Duration::from_secs(30) {
            return Err("the interrupted task never started its turn".to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = i32::try_from(child.id()).map_err(|e| e.to_string())?;
    kill(Pid::from_raw(pid), Signal::SIGINT).map_err(|e| e.to_string())?;
    child.wait_with_output().map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn interrupted(_: Command, _: &Path) -> Result<Output, String> {
    Err("interrupting a run needs POSIX signals".to_owned())
}

/// Whether a trajectory under `dir` recorded `turn.start`.
#[cfg(unix)]
fn turn_started(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            std::fs::read_to_string(entry.path()).is_ok_and(|text| text.contains("\"turn.start\""))
        })
}

fn newest_session(bin: &Path, home: &Path) -> Result<String, String> {
    let out = base(bin, home)
        .args(["--json", "sessions", "list", "--all"])
        .output()
        .map_err(|e| e.to_string())?;
    let listed: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    listed["sessions"][0]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "no session".to_owned())
}
