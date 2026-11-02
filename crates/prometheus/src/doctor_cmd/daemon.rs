//! The `daemon` group: a running daemon must be this build and speak this
//! protocol major, and its served profiles must have a valid configuration
//! (spec: workspace-architecture, Doctor; configuration-and-profiles, Live
//! reload with apply classes).

use std::path::Path;

use pmt_core::doctor::{Check, Status};
use pmt_rpc::control::{ControlRequest, call};
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};

/// Every `daemon` check. Async because it runs inside doctor's runtime:
/// the control call must be awaited there, never wrapped in a nested
/// runtime (tokio panics on that).
pub(super) async fn checks(home: &Path) -> Vec<Check> {
    match discover(home) {
        Ok(Discovery::NotRunning) => vec![running(
            Status::Ok,
            "not running (sessions run in-process)".to_owned(),
        )],
        Ok(Discovery::Starting(process)) => vec![running(
            Status::Ok,
            format!(
                "starting (pid {}, version {})",
                process.pid, process.version
            ),
        )],
        Ok(Discovery::Running(record)) => {
            let mut checks = vec![build(&record)];
            checks.extend(config(&record).await);
            checks
        }
        Err(err) => vec![running(Status::Warn, err.message().to_owned())],
    }
}

fn running(status: Status, message: String) -> Check {
    Check::new("daemon", "running", status, message)
}

/// Whether the daemon runs this build and this protocol major.
fn build(record: &DaemonRecord) -> Check {
    let pid = record.process.pid;
    let version = &record.process.version;
    let ours = pmt_core::protocol::RPC_PROTOCOL;
    let major = record
        .endpoint
        .rpc_protocol
        .split('.')
        .next()
        .and_then(|m| m.parse::<u16>().ok());
    if major != Some(ours.major) {
        return running(
            Status::Fail,
            format!(
                "running protocol {} (pid {pid}), this binary speaks {ours}",
                record.endpoint.rpc_protocol
            ),
        )
        .with_hint("prometheus daemon restart");
    }
    if record.endpoint.build_id != crate::build_info::BUILD_ID {
        return running(
            Status::Warn,
            format!(
                "running another build (pid {pid}, version {version}, build {}), this binary is {} build {}",
                record.endpoint.build_id,
                crate::build_info::VERSION,
                crate::build_info::BUILD_ID
            ),
        )
        .with_hint("prometheus daemon restart");
    }
    running(
        Status::Ok,
        format!("daemon running (pid {pid}, version {version})"),
    )
}

/// What the running daemon reports of each served profile's
/// configuration: an invalid edit it ignored fails, changes waiting for a
/// restart warn.
async fn config(record: &DaemonRecord) -> Vec<Check> {
    let Ok(response) = call(&record.endpoint.control, &ControlRequest::Status).await else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    for profile in response.status.map(|s| s.config).unwrap_or_default() {
        if let Some(error) = &profile.error {
            checks.push(
                Check::new(
                    "daemon",
                    "config",
                    Status::Fail,
                    format!(
                        "{}: the daemon ignored an invalid config.toml and serves the previous one: {error}",
                        profile.profile
                    ),
                )
                .with_hint("fix the file; the daemon reloads it when it is saved"),
            );
        }
        if !profile.pending_restart.is_empty() {
            checks.push(
                Check::new(
                    "daemon",
                    "config",
                    Status::Warn,
                    format!(
                        "{}: changed keys apply after a restart: {}",
                        profile.profile,
                        profile.pending_restart.join(", ")
                    ),
                )
                .with_hint("prometheus daemon restart"),
            );
        }
    }
    checks
}

/// Supervisor units left by an earlier layout or an imported Hermes
/// gateway (spec: messaging-gateway, Per-OS service installation without
/// elevation): reported with the command that replaces each, never removed.
#[cfg(feature = "gateway")]
pub(super) fn legacy_units(
    ctx: &crate::context::Context<'_>,
    home: &std::path::Path,
) -> Vec<Check> {
    crate::daemon_cmd::service::legacy_units(ctx, home)
        .into_iter()
        .map(|unit| {
            Check::new(
                "daemon",
                "legacy_unit",
                Status::Warn,
                format!(
                    "{} left a supervisor unit at {}",
                    unit.origin,
                    unit.path.display()
                ),
            )
            .with_hint(unit.replace)
        })
        .collect()
}

#[cfg(not(feature = "gateway"))]
pub(super) fn legacy_units(_: &crate::context::Context<'_>, _: &std::path::Path) -> Vec<Check> {
    Vec::new()
}
