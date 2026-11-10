//! The liveness fields of the daemon's runtime record `run/gateway.json`
//! (spec: observability, Runtime liveness watchdog and task dump):
//! `state` (`running`, or `degraded` once the watchdog gave up),
//! `updated_at` (refreshed every [`HEARTBEAT`] while the runtime runs) and
//! `exit_reason`. `prometheus status` reads them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_core::runtime::{Role, run_dir};
use serde_json::{Map, Value};

/// Time between two `updated_at` refreshes.
pub(super) const HEARTBEAT: Duration = Duration::from_mins(1);

fn path(home: &Path) -> PathBuf {
    run_dir(home).join(Role::Daemon.record_file())
}

/// Changes the record's fields with `change`; nothing when this process
/// has no record (yet).
fn update(home: &Path, change: impl FnOnce(&mut Map<String, Value>)) {
    let path = path(home);
    let Some(Value::Object(mut record)) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return;
    };
    if record.get("pid").and_then(Value::as_u64) != Some(u64::from(std::process::id())) {
        return;
    }
    change(&mut record);
    if let Ok(json) = serde_json::to_vec_pretty(&Value::Object(record)) {
        let _ = pmt_core::private_fs::write_atomic(&path, &json);
    }
}

fn now() -> Value {
    pmt_obs::trajectory::timestamp(jiff::Timestamp::now()).into()
}

/// Marks the record `running` now.
pub(super) fn running(home: &Path) {
    update(home, |record| {
        record.insert("state".into(), "running".into());
        record.insert("updated_at".into(), now());
        record.remove("exit_reason");
    });
}

/// Marks the record `degraded` with `reason` (the process exits next).
pub(super) fn degraded(home: &Path, reason: &str) {
    update(home, |record| {
        record.insert("state".into(), "degraded".into());
        record.insert("exit_reason".into(), reason.into());
        record.insert("updated_at".into(), now());
    });
}

/// Whether the record already carries the published endpoint (written
/// once the RPC listener is bound; earlier writes would be replaced).
fn published(home: &Path) -> bool {
    std::fs::read(path(home))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|record| record.get("rpc_url").is_some())
}

/// Refreshes `updated_at` every [`HEARTBEAT`] once the endpoint is
/// published; a blocked runtime stops it, which `prometheus status`
/// reports as a stale heartbeat.
pub(super) async fn heartbeat(home: PathBuf) {
    while !published(&home) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    loop {
        running(&home);
        tokio::time::sleep(HEARTBEAT).await;
    }
}

/// The record a new daemon replaces: a degraded predecessor's is
/// forgotten once this one runs.
pub(super) fn forget_last_exit(home: &Path) {
    let _ = std::fs::remove_file(run_dir(home).join(Role::Daemon.last_exit_file()));
}
