//! `prometheus smoke-suite [--output DIR] [--keep]` (spec:
//! replay-and-evaluation, Built-in smoke suite): every bundled task, run
//! by this same binary as a one-shot chat on the mock provider in a
//! scratch home of its own, and graded exactly. It exits 1 when any task
//! misses its grade. A task needing a harness capability this platform
//! lacks is skipped and listed.
//!
//! The suite runs from the binary rather than from cargo so that the
//! Tier 2 offline smoke run can use it where there is no toolchain
//! (Termux); `cargo xtask smoke` runs this command too.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use pmt_core::error::Error;
use serde::Deserialize;

use crate::cli::SmokeSuiteArgs;
use crate::demo_cmd::Place;

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

/// Harness capabilities this platform has. Interrupting a run needs a
/// signal sent from outside; Windows has no way to send Ctrl+C to one
/// child process alone.
fn has(capability: &str) -> bool {
    capability == "posix-signals" && cfg!(unix)
}

/// Runs the suite; exit 0 when every task that ran passed, else 1.
///
/// # Errors
///
/// `demo.output_not_empty` (2), `demo.io` (74) for the scratch directory,
/// and `demo.io` when this binary's path is unknown.
pub fn run(args: &SmokeSuiteArgs) -> Result<u8, Error> {
    let bin = std::env::current_exe().map_err(|e| {
        Error::new(
            &pmt_core::error::codes::DEMO_IO,
            format!("cannot find this binary: {e}"),
        )
    })?;
    let place = Place::prepare(args.output.as_deref())?;
    if !crate::demo_cmd::HAS_OS_SANDBOX {
        println!(
            "note: {} has no OS sandbox; the suite's scripted commands run unconfined in its scratch directory",
            std::env::consts::OS
        );
    }
    let ids = super::task_ids();
    let (mut passed, mut failed, mut skipped) = (0, Vec::new(), Vec::new());
    for id in &ids {
        match run_task(&bin, id, &place.root().join(id)) {
            Outcome::Passed => {
                passed += 1;
                println!("passed: {id}");
            }
            Outcome::Failed(why) => {
                println!("FAILED: {id}: {why}");
                failed.push(*id);
            }
            Outcome::Skipped(needed) => skipped.push(format!("{id} (needs {needed})")),
        }
    }
    println!(
        "smoke: {passed} of {} tasks passed",
        ids.len() - skipped.len()
    );
    for task in &skipped {
        println!("skipped: {task}");
    }
    place.finish(args.keep);
    Ok(if failed.is_empty() {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::FAILURE
    })
}

enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

fn run_task(bin: &Path, id: &str, scratch: &Path) -> Outcome {
    let run: Run = match parse_optional(id, "run.toml") {
        Ok(run) => run.unwrap_or_default(),
        Err(why) => return Outcome::Failed(why),
    };
    if let Some(needed) = run.requires.as_deref().filter(|c| !has(c)) {
        return Outcome::Skipped(needed.to_owned());
    }
    match grade(bin, id, &run, scratch) {
        Ok(()) => Outcome::Passed,
        Err(why) => Outcome::Failed(why),
    }
}

fn read(id: &str, name: &str) -> Result<&'static str, String> {
    super::file(&format!("tasks/{id}/{name}")).ok_or_else(|| format!("no {name}"))
}

fn parse_optional<T: serde::de::DeserializeOwned>(
    id: &str,
    name: &str,
) -> Result<Option<T>, String> {
    super::file(&format!("tasks/{id}/{name}"))
        .map(toml::from_str)
        .transpose()
        .map_err(|e| format!("{name}: {e}"))
}

fn parse<T: serde::de::DeserializeOwned>(id: &str, name: &str) -> Result<T, String> {
    toml::from_str(read(id, name)?).map_err(|e| format!("{name}: {e}"))
}

/// The paths one task works with.
struct Paths<'a> {
    home: &'a Path,
    work: &'a Path,
    mock: &'a Path,
}

/// Runs task `id` in `scratch`; `Err(why)` when it missed its grade or
/// could not run.
fn grade(bin: &Path, id: &str, run: &Run, scratch: &Path) -> Result<(), String> {
    let task: Task = parse(id, "task.toml")?;
    let grader: Grader = parse(id, "grader/grader.toml")?;
    if grader.kind != "exact" {
        return Err(format!("unsupported grader type {}", grader.kind));
    }
    let (home, work, mock) = (
        scratch.join("home"),
        scratch.join("work"),
        scratch.join("mock.toml"),
    );
    let paths = Paths {
        home: &home,
        work: &work,
        mock: &mock,
    };
    prepare(id, run, &task, &paths).map_err(|e| format!("cannot prepare the task: {e}"))?;
    let mut out = chat(bin, &paths, run, &task.prompt, None)?;
    for prompt in &run.then {
        let session = newest_session(bin, &home)?;
        out = chat(bin, &paths, run, prompt, Some(&session))?;
    }
    let actual = match grader.of.as_str() {
        "answer" => String::from_utf8_lossy(&out.stdout).into_owned(),
        "exit" => out.status.code().map(|c| c.to_string()).unwrap_or_default(),
        target => match target.strip_prefix("file:") {
            Some(path) => std::fs::read_to_string(work.join(path)).unwrap_or_default(),
            None => return Err(format!("unknown grader target {target}")),
        },
    };
    // Normalised string equality: surrounding whitespace aside.
    if actual.trim() == grader.expect.trim() {
        return Ok(());
    }
    Err(format!(
        "expected {:?}, got {:?} (stderr: {})",
        grader.expect,
        actual.trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The task's profile config, workspace files and mock script.
fn prepare(id: &str, run: &Run, task: &Task, paths: &Paths<'_>) -> Result<(), String> {
    let profile = paths.home.join("profiles/default");
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(paths.work).map_err(|e| e.to_string())?;
    std::fs::write(profile.join("config.toml"), &run.config).map_err(|e| e.to_string())?;
    for name in &task.files {
        std::fs::write(paths.work.join(name), read(id, &format!("files/{name}"))?)
            .map_err(|e| e.to_string())?;
    }
    std::fs::write(paths.mock, read(id, "mock.toml")?).map_err(|e| e.to_string())
}

/// This binary on the task's home, with none of the invoking
/// environment's Prometheus settings.
fn base(bin: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(bin);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    cmd.arg("--home").arg(home);
    if !crate::demo_cmd::HAS_OS_SANDBOX {
        // As in the demo: the throwaway profile takes the downgrade for
        // its scripted commands; the user's profiles are untouched.
        cmd.args(["--set", r#"security.sandbox.allow_degraded=["all"]"#]);
    }
    cmd
}

/// One one-shot chat of the task.
fn chat(
    bin: &Path,
    paths: &Paths<'_>,
    run: &Run,
    prompt: &str,
    resume: Option<&str>,
) -> Result<Output, String> {
    let mut cmd = base(bin, paths.home);
    for set in &run.set {
        cmd.args(["--set", set]);
    }
    cmd.args(["chat", "-z", prompt, "--provider", "mock", "--mock-script"])
        .arg(paths.mock)
        .current_dir(paths.work)
        .stdin(Stdio::null());
    if let Some(id) = resume {
        cmd.args(["-r", id]);
    }
    if !run.interrupt_after_turn_start {
        return cmd.output().map_err(|e| e.to_string());
    }
    interrupted(cmd, paths.home)
}

/// Starts `cmd`, sends it SIGINT once its turn has started, and waits.
#[cfg(unix)]
fn interrupted(mut cmd: Command, home: &Path) -> Result<Output, String> {
    use std::time::{Duration, Instant};

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
