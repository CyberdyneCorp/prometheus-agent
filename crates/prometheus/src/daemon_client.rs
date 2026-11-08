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

/// The `rpc.hello` of this binary as a client of kind `client`.
#[must_use]
pub fn hello(client: ClientKind, server_requests: bool) -> HelloParams {
    HelloParams {
        client,
        client_version: crate::build_info::VERSION.to_owned(),
        protocol: Version::from(pmt_core::protocol::RPC_PROTOCOL),
        capabilities: ClientCapabilities {
            server_requests,
            events: Vec::new(),
        },
    }
}

/// Attaches to `record`'s daemon for `profile` with an interactive ticket
/// and performs `hello`; also returns the stale-build notice when the
/// daemon runs another build. Every client (CLI, TUI) connects through
/// here so the handshake and the stale-build policy live in one place.
///
/// # Errors
///
/// `rpc.ticket` (the daemon does not serve `profile`), `rpc.connection`,
/// `rpc.protocol_mismatch` (76).
pub async fn connect(
    record: &DaemonRecord,
    profile: &str,
    hello: &HelloParams,
) -> Result<(Client, Inbox, Option<String>), Error> {
    let (client, inbox) = Client::connect(
        &record.endpoint,
        Purpose::Interactive,
        vec![profile.to_owned()],
        hello,
    )
    .await?;
    let notice = stale_build_notice(client.hello(), crate::build_info::BUILD_ID);
    Ok((client, inbox, notice))
}

/// [`connect`] for a line-oriented CLI command: the notice goes to stderr.
///
/// # Errors
///
/// As [`connect`].
pub async fn connect_cli(
    record: &DaemonRecord,
    profile: &str,
    hello: &HelloParams,
) -> Result<(Client, Inbox), Error> {
    let (client, inbox, notice) = connect(record, profile, hello).await?;
    if let Some(notice) = notice {
        eprintln!("{notice}");
    }
    Ok((client, inbox))
}

/// The memory review summary of `session` (spec: memory, Reviewable
/// memory changes per session), for a client to show as the session
/// ends or is resumed; `None` when there is none or the daemon cannot
/// say (an older daemon, a lost connection): a missing line never fails
/// the client.
pub async fn memory_summary(client: &Client, session: &str) -> Option<String> {
    let params = pmt_rpc::contracts::SessionRef {
        session_id: session.to_owned(),
    };
    match client
        .call_typed::<pmt_rpc::contracts::MemorySummaryCall>(&params)
        .await
    {
        Ok(summary) => summary.line,
        Err(err) => {
            tracing::debug!(error = %err.message, "no memory summary");
            None
        }
    }
}

/// The low-disk prune question when it is due at this interactive start
/// (spec: session-storage, Low-disk prune prompt); answered with
/// `/autoprune on|off|later`. `None` when not due or the daemon cannot
/// say.
pub async fn autoprune_offer(client: &Client) -> Option<String> {
    let params = pmt_rpc::contracts::AutopruneOfferParams::default();
    match client
        .call_typed::<pmt_rpc::contracts::AutopruneOfferCall>(&params)
        .await
    {
        Ok(offer) => offer.question,
        Err(err) => {
            tracing::debug!(error = %err.message, "no low-disk check");
            None
        }
    }
}

/// The `allow_roles` hint of config validation (spec: messaging-gateway,
/// Role-based access grants): a role name is looked up by the home's
/// running daemon, whose connected adapter may know its id. Never asked
/// from inside an async runtime (the daemon validating its own reload),
/// and only of a daemon that is already running.
#[must_use]
pub fn role_resolver() -> pmt_config::gateway_check::RoleResolver {
    Box::new(|profile_dir: &Path, platform: &str, name: &str| {
        if tokio::runtime::Handle::try_current().is_ok() {
            return None;
        }
        let profile = profile_dir.file_name()?.to_string_lossy().into_owned();
        let home = profile_dir.parent()?.parent()?;
        let Ok(Discovery::Running(record)) = discover(home) else {
            return None;
        };
        let request = pmt_rpc::control::ControlRequest::ResolveRole {
            profile,
            platform: platform.to_owned(),
            name: name.to_owned(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let control = record.endpoint.control;
        runtime
            .block_on(pmt_rpc::control::call(&control, &request))
            .ok()?
            .resolved
    })
}
