//! What `prometheus status` knows about the home's daemon (spec:
//! observability, Status command; Runtime liveness watchdog and task
//! dump): its control-channel status, the liveness fields of its runtime
//! record, or the record a degraded predecessor left.

use std::path::Path;
use std::time::Duration;

use pmt_core::error::Error;
use pmt_core::runtime::{Role, run_dir};
use pmt_rpc::control::{self, ControlRequest, DaemonStatus};
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};
use serde::Serialize;
use serde_json::Value;

/// A heartbeat older than this, with the pid alive, is stale.
pub(super) const STALE_AFTER: Duration = Duration::from_mins(2);

/// The daemon as `status` reports it.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DaemonFacts {
    /// It runs.
    pub running: bool,
    /// Its control-channel status (when it answered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<DaemonStatus>,
    /// It runs this binary's build.
    pub build_match: bool,
    /// `running` or `degraded` (the runtime record's `state`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Seconds since the record's `updated_at`, when older than allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_stale_secs: Option<u64>,
    /// The health listener.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_url: Option<String>,
    /// A degraded predecessor's record (`state`, `exit_reason`,
    /// `updated_at`), until a new daemon replaces it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<Value>,
}

/// Looks the daemon of `home` up and asks it for its status.
///
/// # Errors
///
/// `home.io` reading the discovery file.
pub(super) fn facts(home: &Path) -> Result<DaemonFacts, Error> {
    let record = match discover(home)? {
        Discovery::Running(record) => Some(record),
        Discovery::Starting(_) | Discovery::NotRunning => None,
    };
    let last_exit = read_json(&run_dir(home).join(Role::Daemon.last_exit_file()));
    let Some(record) = record else {
        return Ok(DaemonFacts {
            running: false,
            status: None,
            build_match: false,
            state: None,
            heartbeat_stale_secs: None,
            health_url: None,
            last_exit,
        });
    };
    let raw = read_json(&run_dir(home).join(Role::Daemon.record_file())).unwrap_or_default();
    let status = ask(&record);
    Ok(DaemonFacts {
        running: true,
        build_match: record.endpoint.build_id == crate::build_info::BUILD_ID,
        state: raw["state"].as_str().map(str::to_owned),
        heartbeat_stale_secs: stale(raw["updated_at"].as_str()),
        health_url: record.endpoint.health_url.clone(),
        status,
        last_exit,
    })
}

fn ask(record: &DaemonRecord) -> Option<DaemonStatus> {
    let control = record.endpoint.control.clone();
    crate::model_cmd::block_on(
        async move { control::call(&control, &ControlRequest::Status).await },
    )
    .ok()
    .and_then(Result::ok)
    .and_then(|response| response.status)
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Seconds since `updated_at` when beyond [`STALE_AFTER`].
fn stale(updated_at: Option<&str>) -> Option<u64> {
    let at: jiff::Timestamp = updated_at?.parse().ok()?;
    let age = jiff::Timestamp::now().duration_since(at).as_secs();
    let age = u64::try_from(age).unwrap_or(0);
    (age > STALE_AFTER.as_secs()).then_some(age)
}

/// `GET <health_url>/healthz` within 2 s: whether it answered 200.
pub(super) fn healthz(health_url: &str) -> bool {
    let addr = health_url
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(addr) else {
        return false;
    };
    let Some(addr) = addrs.into_iter().next() else {
        return false;
    };
    let within = Duration::from_secs(2);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, within) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(within));
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if std::io::Write::write_all(&mut stream, request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = std::io::Read::read_to_string(&mut stream, &mut response);
    response.starts_with("HTTP/1.1 200")
}
