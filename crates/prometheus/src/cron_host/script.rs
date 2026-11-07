//! Job scripts (spec: scheduled-jobs, Script-backed and no-agent jobs;
//! Script wake gate and script credentials): run under the profile's
//! process sandbox (its capability baseline, the scripts directory
//! readable) from the job's working directory (else the cron workspace),
//! with a sanitized environment holding only the job's `script_secrets`,
//! killed past `cron.script_timeout_secs`. `.sh`/`.bash` files run with
//! bash, `.py` files with the managed Python runtime or the job's
//! `interpreter`; other files run as executables.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_cron::exec::ScriptOutcome;
use pmt_cron::job::Job;

/// How often a running script is checked.
const POLL: Duration = Duration::from_millis(50);

/// Runs `script` of `job` and collects its output.
pub(super) async fn run(
    scope: &ProfileScope,
    job: &Job,
    script: &Path,
    timeout: Duration,
) -> ScriptOutcome {
    let scope = scope.clone();
    let job = job.clone();
    let script = script.to_path_buf();
    tokio::task::spawn_blocking(move || run_blocking(&scope, &job, &script, timeout))
        .await
        .unwrap_or_else(|e| failed(format!("the script runner panicked: {e}")))
}

fn failed(message: String) -> ScriptOutcome {
    ScriptOutcome {
        exit_code: None,
        stderr: message,
        ..ScriptOutcome::default()
    }
}

/// The first `name` on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The program that runs `script`, and whether the script is its
/// argument (`false`: the script is the program).
fn program(scope: &ProfileScope, job: &Job, script: &Path) -> Result<(PathBuf, bool), String> {
    let extension = script.extension().and_then(|e| e.to_str()).unwrap_or("");
    match extension {
        "sh" | "bash" => on_path("bash")
            .map(|bash| (bash, true))
            .ok_or_else(|| "bash is not installed".to_owned()),
        "py" => {
            if let Some(interpreter) = &job.spec.interpreter {
                return Ok((PathBuf::from(interpreter), true));
            }
            let version = scope.config().python_bridge.python_version;
            pmt_pybridge::runtime::find(scope.home(), version)
                .map(|python| (python.exe, true))
                .ok_or_else(|| {
                    "no managed Python runtime is installed: run `prometheus runtimes install \
                     python` or set the job's interpreter"
                        .to_owned()
                })
        }
        _ => Ok((script.to_path_buf(), false)),
    }
}

/// The working directory of the job's runs: its `workdir`, else the
/// cron workspace.
pub(super) fn cwd(scope: &ProfileScope, job: &Job) -> PathBuf {
    job.spec
        .workdir
        .as_ref()
        .map_or_else(|| super::workspace(scope), PathBuf::from)
}

fn run_blocking(
    scope: &ProfileScope,
    job: &Job,
    script: &Path,
    timeout: Duration,
) -> ScriptOutcome {
    let dir = cwd(scope, job);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return failed(format!("cannot create {}: {e}", dir.display()));
    }
    let (program, as_argument) = match program(scope, job, script) {
        Ok(found) => found,
        Err(message) => return failed(message),
    };
    let cwd = dir.to_string_lossy().into_owned();
    let scripts = [script.to_path_buf()];
    let mut command = match crate::hooks_host::sandboxed(scope, &program, &scripts, Some(&cwd)) {
        Ok(command) => command,
        Err(err) => return failed(err.message().to_owned()),
    };
    if as_argument {
        command.arg(script);
    }
    for name in &job.spec.script_secrets {
        match scope.secrets().resolve(name) {
            Ok(value) => {
                command.env(name, value.expose());
            }
            Err(err) => return failed(format!("script secret '{name}': {}", err.message())),
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return failed(format!("cannot start {}: {e}", script.display())),
    };
    let stdout = reader(child.stdout.take());
    let stderr = reader(child.stderr.take());
    let started = Instant::now();
    let (exit_code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => return failed(format!("waiting for the script failed: {e}")),
        }
    };
    if timed_out {
        // A killed script's own children may still hold its pipes: its
        // output is not waited for.
        return ScriptOutcome {
            timed_out,
            ..ScriptOutcome::default()
        };
    }
    ScriptOutcome {
        exit_code,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        timed_out,
    }
}

fn reader(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}
