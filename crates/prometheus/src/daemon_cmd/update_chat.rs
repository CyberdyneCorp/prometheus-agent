//! The daemon's side of a confirmed `/update` (spec: distribution-and-
//! update, Update from chat): the same code path as `prometheus update`,
//! run as a detached child process of this binary so it outlives the
//! daemon it replaces. `/update check` runs `update --check` and answers
//! with what it printed.

use std::path::Path;
use std::process::{Command, Stdio};

use pmt_core::error::{Error, codes};

/// Where the detached updater writes its output.
fn log_path(home: &Path) -> std::path::PathBuf {
    home.join("logs").join("update-from-chat.log")
}

fn failed(what: &str, e: &std::io::Error) -> Error {
    Error::new(&codes::RPC_INTERNAL, format!("{what}: {e}"))
}

/// Runs the updater for `home`; the text answers the chat.
///
/// # Errors
///
/// When the updater cannot be started (or, for a check, fails).
pub(super) fn run(home: &Path, check: bool) -> Result<String, Error> {
    let exe = std::env::current_exe().map_err(|e| failed("cannot find this binary", &e))?;
    let mut command = Command::new(exe);
    command.arg("--home").arg(home).arg("update");
    if check {
        let out = command
            .arg("--check")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed("the update check did not start", &e))?;
        let text = String::from_utf8_lossy(if out.status.success() {
            &out.stdout
        } else {
            &out.stderr
        })
        .trim()
        .to_owned();
        return Ok(text);
    }
    let log = log_path(home);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| failed("cannot create the log directory", &e))?;
    }
    let file =
        std::fs::File::create(&log).map_err(|e| failed("cannot create the update log", &e))?;
    let err = file
        .try_clone()
        .map_err(|e| failed("cannot create the update log", &e))?;
    command
        .arg("--yes")
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err);
    super::detach(&mut command);
    command
        .spawn()
        .map_err(|e| failed("the updater did not start", &e))?;
    Ok(format!(
        "Update started; the daemon restarts once the new version is in place (log: {}).",
        log.display()
    ))
}
