//! Runs another `prometheus` binary — the staged build's `self-test`, the
//! previous build's `version --json` — with a deadline (spec:
//! distribution-and-update, Atomic binary swap with health check).

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use serde::Deserialize;

/// The staged build's self-test budget.
pub const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What `self-test --json` and `version --json` both report.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BuildInfo {
    /// The build's version (Cargo form, no `v`).
    pub version: String,
    /// Newest `state.db` schema it reads.
    pub store_schema_version: u32,
    /// `config.toml` schema it writes.
    pub config_schema_version: u32,
}

/// Runs `<binary> --home <home> self-test --json`.
///
/// # Errors
///
/// `update.self_test_failed` (exit 1) `staged build failed self-test:
/// <reason>` when it exits non-zero, times out or prints no report.
pub fn self_test(binary: &Path, home: &Path) -> Result<BuildInfo, Error> {
    let failed = |reason: String| {
        Error::new(
            &codes::UPDATE_SELF_TEST_FAILED,
            format!("staged build failed self-test: {reason}"),
        )
        .with_hint("the installed binary was not changed")
    };
    let mut command = Command::new(binary);
    command
        .arg("--home")
        .arg(home)
        .args(["self-test", "--json"]);
    let output = run(command, SELF_TEST_TIMEOUT).map_err(failed)?;
    serde_json::from_slice(&output).map_err(|e| failed(format!("unreadable report: {e}")))
}

/// Runs `<binary> version --json`.
///
/// # Errors
///
/// `update.rollback_refused` when the binary cannot report its version.
pub fn version(binary: &Path) -> Result<BuildInfo, Error> {
    let failed = |reason: String| {
        Error::new(
            &codes::UPDATE_ROLLBACK_REFUSED,
            format!(
                "cannot roll back: {} does not run: {reason}",
                binary.display()
            ),
        )
    };
    let mut command = Command::new(binary);
    command.args(["version", "--json"]);
    let output = run(command, SELF_TEST_TIMEOUT).map_err(failed)?;
    serde_json::from_slice(&output).map_err(|e| failed(e.to_string()))
}

/// Runs `command`, returns its stdout when it exits 0 within `timeout`,
/// else a one-line reason.
fn run(mut command: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start it: {e}"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let text = String::from_utf8_lossy(&stderr);
    let last = text.lines().rev().find(|l| !l.trim().is_empty());
    Err(last.map_or_else(
        || format!("it exited with {status}"),
        |l| l.trim().to_owned(),
    ))
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::{run, self_test};

    fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("prometheus");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn failing_self_test_reports_its_last_stderr_line() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "echo 'sandbox probe failed' >&2; exit 3");
        let err = self_test(&bin, dir.path()).unwrap_err();
        assert_eq!(
            err.message(),
            "staged build failed self-test: sandbox probe failed"
        );
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn hanging_build_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "sleep 5");
        let reason = run(std::process::Command::new(bin), Duration::from_millis(200)).unwrap_err();
        assert_eq!(reason, "no answer within 0 s");
    }
}
