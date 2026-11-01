//! What every CLI client of the daemon shares (spec: workspace-architecture,
//! Clients attach to a running daemon; Stale daemon build detection): find
//! the home's running daemon, get a ticket for one profile and perform the
//! handshake as a `cli` client.

use std::path::Path;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_rpc::client::{Client, Inbox, stale_build_notice};
use pmt_rpc::contracts::{ClientCapabilities, ClientKind, HelloParams, Version};
use pmt_rpc::control::Purpose;
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};

/// How long a client waits for a daemon that is starting to publish its
/// endpoint before treating it as absent.
const STARTING_WAIT: Duration = Duration::from_secs(5);

/// The running daemon of `home`; `None` when none runs. A daemon that is
/// still starting gets a few seconds to publish its endpoint.
///
/// # Errors
///
/// `home.io` when the discovery file cannot be read.
pub fn running(home: &Path) -> Result<Option<DaemonRecord>, Error> {
    let deadline = std::time::Instant::now() + STARTING_WAIT;
    loop {
        match discover(home)? {
            Discovery::Running(record) => return Ok(Some(record)),
            Discovery::NotRunning => return Ok(None),
            Discovery::Starting(_) if std::time::Instant::now() > deadline => return Ok(None),
            Discovery::Starting(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// The running daemon of `home`, or `daemon.not_running` (exit 69).
///
/// # Errors
///
/// `daemon.not_running`, `home.io`.
pub fn require(home: &Path) -> Result<DaemonRecord, Error> {
    running(home)?.ok_or_else(|| {
        Error::new(
            &codes::DAEMON_NOT_RUNNING,
            format!("no gateway daemon runs for {}", home.display()),
        )
        .with_hint("start it with: prometheus daemon start")
    })
}

/// The `rpc.hello` of this binary as a `cli` client.
#[must_use]
pub fn hello(server_requests: bool) -> HelloParams {
    HelloParams {
        client: ClientKind::Cli,
        client_version: crate::build_info::VERSION.to_owned(),
        protocol: Version::from(pmt_core::protocol::RPC_PROTOCOL),
        capabilities: ClientCapabilities {
            server_requests,
            events: Vec::new(),
        },
    }
}

/// Attaches to `record`'s daemon for `profile` with an interactive ticket
/// and performs `hello`; prints the stale-build notice on stderr when the
/// daemon runs another build.
///
/// # Errors
///
/// `rpc.ticket` (the daemon does not serve `profile`), `rpc.connection`,
/// `rpc.protocol_mismatch` (76).
pub async fn connect(
    record: &DaemonRecord,
    profile: &str,
    hello: &HelloParams,
) -> Result<(Client, Inbox), Error> {
    let (client, inbox) = Client::connect(
        &record.endpoint,
        Purpose::Interactive,
        vec![profile.to_owned()],
        hello,
    )
    .await?;
    if let Some(notice) = stale_build_notice(client.hello(), crate::build_info::BUILD_ID) {
        eprintln!("{notice}");
    }
    Ok((client, inbox))
}
