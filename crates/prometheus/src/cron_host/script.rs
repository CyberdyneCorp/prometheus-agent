//! Job scripts (spec: scheduled-jobs, Script-backed and no-agent jobs):
//! run under the profile's process sandbox (its capability baseline, the
//! scripts directory readable) from the cron workspace, killed past
//! `cron.script_timeout_secs`.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_cron::exec::ScriptOutcome;

/// How often a running script is checked.
const POLL: Duration = Duration::from_millis(50);

/// Runs `script` and collects its output.
pub(super) async fn run(scope: &ProfileScope, script: &Path, timeout: Duration) -> ScriptOutcome {
    let scope = scope.clone();
    let script = script.to_path_buf();
    tokio::task::spawn_blocking(move || run_blocking(&scope, &script, timeout))
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

fn run_blocking(scope: &ProfileScope, script: &Path, timeout: Duration) -> ScriptOutcome {
    let workspace = super::workspace(scope);
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        return failed(format!("cannot create {}: {e}", workspace.display()));
    }
    let cwd = workspace.to_string_lossy().into_owned();
    let mut command = match crate::hooks_host::sandboxed(scope, script, &[], Some(&cwd)) {
        Ok(command) => command,
        Err(err) => return failed(err.message().to_owned()),
    };
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
