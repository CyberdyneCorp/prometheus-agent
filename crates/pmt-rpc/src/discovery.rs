//! The daemon's discovery file `<home>/run/gateway.json` (spec:
//! workspace-architecture, Daemon discovery file and local control
//! channel): the runtime record of the daemon process plus its endpoint.
//! A record whose pid is not alive (or was reused) is stale: clients treat
//! the daemon as not running and never dial its `rpc_url`.

use std::path::Path;

use pmt_core::error::Error;
use pmt_core::runtime::{Role, RuntimeRecord, live_record, run_dir};
use serde::{Deserialize, Serialize};

/// Where and how to reach a running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Random per daemon start; tickets are bound to it.
    pub instance_id: String,
    /// The daemon binary's build id.
    pub build_id: String,
    /// The loopback WebSocket URL (`ws://127.0.0.1:<port>/rpc`).
    pub rpc_url: String,
    /// The control channel: a Unix socket path, or a named pipe on Windows.
    pub control: String,
    /// The protocol, `<major>.<minor>`.
    pub rpc_protocol: String,
    /// When the daemon started (RFC 3339).
    pub started_at: String,
    /// The loopback health listener (`http://127.0.0.1:<port>`: `/healthz`,
    /// `/readyz`, `/metrics`), when it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_url: Option<String>,
    /// The protocol features the daemon serves ([`FEATURES`] for this
    /// build).
    #[serde(default)]
    pub features: Vec<String>,
}

/// The protocol features of this build's daemon (spec: desktop-app, Local
/// endpoint validation): the session-attachment semantics of the `tui`
/// capability.
pub const FEATURES: &[&str] = &["session-attach"];

/// A running daemon's discovery record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// The process (pid, start time, version, profiles).
    #[serde(flatten)]
    pub process: RuntimeRecord,
    /// The endpoint.
    #[serde(flatten)]
    pub endpoint: Endpoint,
}

/// What a client finds for a home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// No live daemon (a stale record was removed).
    NotRunning,
    /// A live daemon that has not published its endpoint yet.
    Starting(RuntimeRecord),
    /// A live daemon and where to reach it.
    Running(DaemonRecord),
}

/// Looks for the daemon of `home`.
///
/// # Errors
///
/// `home.io` when the record cannot be read or a stale one removed.
pub fn discover(home: &Path) -> Result<Discovery, Error> {
    let Some(process) = live_record(home, Role::Daemon)? else {
        return Ok(Discovery::NotRunning);
    };
    let path = run_dir(home).join(Role::Daemon.record_file());
    let record = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DaemonRecord>(&bytes).ok())
        .filter(|record| record.process == process);
    Ok(match record {
        Some(record) => Discovery::Running(record),
        None => Discovery::Starting(process),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pmt_core::runtime::{Role, RuntimeRecord, run_dir};

    use super::{DaemonRecord, Discovery, Endpoint, discover};

    fn endpoint(rpc_url: String) -> Endpoint {
        Endpoint {
            instance_id: "i".into(),
            build_id: "b".into(),
            rpc_url,
            control: "c".into(),
            rpc_protocol: "1.0".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            health_url: None,
            features: Vec::new(),
        }
    }

    fn write(home: &std::path::Path, record: &DaemonRecord) {
        std::fs::create_dir_all(run_dir(home)).unwrap();
        let json = serde_json::to_vec(record).unwrap();
        std::fs::write(run_dir(home).join("gateway.json"), json).unwrap();
    }

    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[tokio::test]
    async fn a_stale_discovery_file_is_not_running_and_never_dialled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/rpc", listener.local_addr().unwrap());
        let home = tempfile::tempdir().unwrap();
        let mut process = RuntimeRecord::current(Role::Daemon, vec!["default".into()]);
        process.pid = dead_pid();
        write(
            home.path(),
            &DaemonRecord {
                process,
                endpoint: endpoint(url),
            },
        );
        assert_eq!(discover(home.path()).unwrap(), Discovery::NotRunning);
        assert!(!run_dir(home.path()).join("gateway.json").exists());
        let dialled = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(dialled.is_err(), "nothing dialled the stale rpc_url");
    }

    #[test]
    fn a_live_record_with_an_endpoint_is_running() {
        let home = tempfile::tempdir().unwrap();
        let process = RuntimeRecord::current(Role::Daemon, vec![]);
        let record = DaemonRecord {
            process: process.clone(),
            endpoint: endpoint("ws://127.0.0.1:1/rpc".into()),
        };
        write(home.path(), &record);
        assert_eq!(discover(home.path()).unwrap(), Discovery::Running(record));
        std::fs::write(
            run_dir(home.path()).join("gateway.json"),
            serde_json::to_vec(&process).unwrap(),
        )
        .unwrap();
        assert_eq!(discover(home.path()).unwrap(), Discovery::Starting(process));
    }
}
