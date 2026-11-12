//! A daemon process on a test home, stopped when the guard drops.

use std::path::Path;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::{ClientCapabilities, ClientKind, HelloParams, Version};
use pmt_rpc::control::Purpose;
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};

use super::{ok, run};

/// Stops the home's daemon when the test ends, whatever happened.
pub struct Daemon<'a> {
    pub home: &'a Path,
    pub pid: u32,
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = run(self.home, &["daemon", "stop"]);
    }
}

/// Starts the daemon of `home` (`extra` are global flags such as `-p x`).
pub fn start<'a>(home: &'a Path, extra: &[&str]) -> Daemon<'a> {
    let mut args = extra.to_vec();
    args.extend(["daemon", "start"]);
    let text = ok(&run(home, &args));
    assert!(text.starts_with("daemon started (pid "), "{text}");
    Daemon {
        home,
        pid: record(home).process.pid,
    }
}

/// The running daemon's discovery record.
pub fn record(home: &Path) -> DaemonRecord {
    match discover(home).unwrap() {
        Discovery::Running(record) => record,
        other => panic!("no daemon: {other:?}"),
    }
}

/// A `cli` hello.
pub fn hello(server_requests: bool) -> HelloParams {
    HelloParams {
        client: ClientKind::Cli,
        client_version: env!("CARGO_PKG_VERSION").into(),
        protocol: Version { major: 1, minor: 0 },
        capabilities: ClientCapabilities {
            server_requests,
            events: Vec::new(),
        },
    }
}

/// An interactive client of `profile` (call inside a runtime).
pub async fn connect(home: &Path, profile: &str) -> (Client, Inbox) {
    Client::connect(
        &record(home).endpoint,
        Purpose::Interactive,
        vec![profile.into()],
        &hello(false),
    )
    .await
    .unwrap()
}

/// The port the running daemon's listener `component` (health component,
/// e.g. `listener:api_server`) is bound to, once it listens.
///
/// Tests configure their listeners on port 0 and read the port back here:
/// a port picked free and released by the test can be bound by another
/// test's daemon (or mock server) before this daemon binds it, and every
/// test home's profile has the same API key, so the requests would be
/// answered by the wrong daemon.
pub fn bound_port(home: &Path, component: &str) -> u16 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = run(home, &["--json", "daemon", "status"]);
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let listener = status["daemon"]["health"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["component"] == component);
        let reason = listener.and_then(|c| c["reason"].as_str());
        if let Some(addr) = reason.and_then(|r| r.strip_prefix("listening on ")) {
            return addr.parse::<std::net::SocketAddr>().unwrap().port();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{component} never listened: {listener:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
