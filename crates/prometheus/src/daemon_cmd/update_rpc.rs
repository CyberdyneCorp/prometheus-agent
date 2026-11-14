//! The daemon's side of `daemon.update` (spec: desktop-app, Update every
//! connected instance): the same code path as `prometheus update`, run as
//! a detached child of this binary (it holds the update lock and hands
//! this daemon over to the new build), its output in
//! `logs/update-rpc.log`. An install another owner updates (a package
//! manager, the container image, a source build) is refused at once with
//! the updater's own `update.owned_elsewhere` message, so the asking
//! client shows the owner's refusal instead of a started update.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::DaemonUpdated;
use pmt_update::Owner;

fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("update-rpc.log")
}

fn failed(what: &str, e: &dyn std::fmt::Display) -> Error {
    Error::new(&codes::RPC_INTERNAL, format!("{what}: {e}"))
}

fn updater(home: &Path) -> Result<Command, Error> {
    let exe = std::env::current_exe().map_err(|e| failed("cannot find this binary", &e))?;
    let mut command = Command::new(exe);
    command.arg("--home").arg(home);
    Ok(command)
}

/// Runs `update --check`, or starts the update detached.
///
/// # Errors
///
/// `update.owned_elsewhere` for an install another owner updates;
/// `rpc.internal` when the updater cannot run.
pub(super) fn run(home: &Path, check: bool) -> Result<DaemonUpdated, Error> {
    refuse_other_owners(home)?;
    if check {
        let out = updater(home)?
            .args(["update", "--check"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed("the update check did not start", &e))?;
        // Exit 10: an update is available.
        let text = if out.status.success() || out.status.code() == Some(10) {
            &out.stdout
        } else {
            &out.stderr
        };
        return Ok(DaemonUpdated {
            status: "checked".to_owned(),
            message: String::from_utf8_lossy(text).trim().to_owned(),
        });
    }
    let log = log_path(home);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| failed("cannot create the log directory", &e))?;
    }
    let file = std::fs::File::create(&log).map_err(|e| failed("cannot create the log", &e))?;
    let err = file
        .try_clone()
        .map_err(|e| failed("cannot create the log", &e))?;
    let mut command = updater(home)?;
    // `--yes`: the client asked for this update; a busy daemon is drained
    // by the handover as `prometheus update --yes` does.
    command
        .args(["update", "--yes"])
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err);
    super::detach(&mut command);
    let child = command
        .spawn()
        .map_err(|e| failed("the updater did not start", &e))?;
    // Reaped by a thread so it never lingers as a zombie of this daemon
    // (the updater replaces the daemon, which usually exits first).
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(DaemonUpdated {
        status: "started".to_owned(),
        message: format!("updating; the log is {}", log.display()),
    })
}

/// The owner refusal `prometheus update` gives, read from `update --status`
/// so the receipt rules stay the updater's own.
fn refuse_other_owners(home: &Path) -> Result<(), Error> {
    let out = updater(home)?
        .args(["--json", "update", "--status"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| failed("the update status did not run", &e))?;
    let status: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| failed("the update status did not parse", &e))?;
    let owner = status["owner"].as_str().and_then(Owner::parse);
    match owner {
        Some(owner) if owner.self_updates() => Ok(()),
        _ => {
            let name = status["owner"].as_str().unwrap_or("dev");
            let with = status["update_with"]
                .as_str()
                .unwrap_or("rebuild from source");
            Err(Error::new(
                &codes::UPDATE_OWNED_ELSEWHERE,
                format!("owner: {name}: this install is not updated by prometheus update"),
            )
            .with_hint(format!("update with: {with}")))
        }
    }
}
