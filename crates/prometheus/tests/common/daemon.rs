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
