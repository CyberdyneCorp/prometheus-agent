//! The TUI's side of the client protocol (spec: tui, Hello handshake is
//! mandatory and versioned; Session access requires an attachment; Event
//! sequencing and reconnect replay; Idempotent session creation; Profile
//! scope on every request): the `tui` handshake, opening the session as
//! its interactive owner (created with an idempotency key, or the one
//! `-r`/`-c` names), the event watermark, and re-attaching after a lost
//! connection with that watermark, reloading the transcript when the
//! daemon says the replay is `truncated` (another epoch, or evicted).

use pmt_core::error::Error;
use pmt_rpc::client::{Client, Inbox, error_of, stale_build_notice};
use pmt_rpc::contracts::{
    AttachParams, AttachRole, Attached, ClientCapabilities, ClientKind, CommandEntry,
    CommandsCatalog, Disposition, Envelope, ForkParams, HelloParams, ResolveKind, ResolveParams,
    ResumeParams, SessionAttach, SessionCreate, SessionCreateParams, SessionDetach, SessionFork,
    SessionRef, SessionResolve, SessionResume, SessionSetup, SessionTranscript, Transcript,
    Version,
};
use pmt_rpc::control::Purpose;
use pmt_rpc::discovery::DaemonRecord;

use super::Launch;

/// The `rpc.hello` of the TUI: it answers server requests.
pub(super) fn hello() -> HelloParams {
    HelloParams {
        client: ClientKind::Tui,
        client_version: crate::build_info::VERSION.to_owned(),
        protocol: Version::from(pmt_core::protocol::RPC_PROTOCOL),
        capabilities: ClientCapabilities {
            server_requests: true,
            events: Vec::new(),
        },
    }
}

fn rpc(err: &pmt_rpc::RpcError) -> Error {
    error_of(err)
}

/// A connection, the session the screen shows and where its events are.
pub(super) struct Link {
    /// The connection.
    pub(super) client: Client,
    /// Its events and server requests.
    pub(super) inbox: Inbox,
    /// The session shown.
    pub(super) session: String,
    /// The daemon epoch of `last_seq`.
    pub(super) epoch: String,
    /// The newest event sequence number applied.
    pub(super) last_seq: u64,
    /// The session's commands (`commands.catalog`).
    pub(super) commands: Vec<CommandEntry>,
    /// The profile the ticket grants.
    pub(super) profile: String,
    /// The daemon.
    pub(super) record: DaemonRecord,
}

/// Connects to `record`'s daemon for `profile`; also returns the
/// stale-build notice when the daemon runs another build.
///
/// # Errors
///
/// `rpc.ticket`, `rpc.connection`, `rpc.protocol_mismatch` (76).
pub(super) async fn connect(
    record: &DaemonRecord,
    profile: &str,
) -> Result<(Client, Inbox, Option<String>), Error> {
    let (client, inbox) = Client::connect(
        &record.endpoint,
        Purpose::Interactive,
        vec![profile.to_owned()],
        &hello(),
    )
    .await?;
    let notice = stale_build_notice(client.hello(), crate::build_info::BUILD_ID);
    Ok((client, inbox, notice))
}

impl Link {
    /// Connects and opens the session `launch` names (or a new one).
    ///
    /// # Errors
    ///
    /// Connection errors, `session.in_use` (75), resolution and creation
    /// errors.
    pub(super) async fn open(
        record: DaemonRecord,
        profile: &str,
        launch: &Launch,
    ) -> Result<(Self, Option<String>), Error> {
        let (client, inbox, notice) = connect(&record, profile).await?;
        let epoch = client.hello().epoch.clone();
        let mut link = Self {
            client,
            inbox,
            session: String::new(),
            epoch,
            last_seq: 0,
            commands: Vec::new(),
            profile: profile.to_owned(),
            record,
        };
        // A new session is loaded at once too, so its commands (skills
        // included) list.
        let (session, setup) = if let Some(found) = link.target(launch).await? {
            (found, Some(&launch.setup))
        } else {
            (create(&link.client, launch).await?, None)
        };
        let attached = resume(&link.client, &session, setup).await?;
        link.watermark(&attached);
        link.session = session;
        link.refresh_commands().await;
        Ok((link, notice))
    }

    /// The stored session `-r`/`-c` names.
    async fn target(&self, launch: &Launch) -> Result<Option<String>, Error> {
        let (kind, reference) = match (&launch.resume, &launch.continue_) {
            (Some(reference), _) => (ResolveKind::Resume, reference.clone()),
            (None, Some(name)) => (ResolveKind::Continue, name.clone()),
            (None, None) => return Ok(None),
        };
        resolve(&self.client, kind, reference, launch)
            .await
            .map(Some)
    }

    /// Detaches from the shown session and shows `session` (attached as
    /// `attached` says) instead.
    pub(super) async fn switch(&mut self, session: String, attached: &Attached) {
        let old = SessionRef {
            session_id: std::mem::replace(&mut self.session, session),
        };
        if old.session_id != self.session {
            let _ = self.client.call_typed::<SessionDetach>(&old).await;
        }
        self.watermark(attached);
        self.refresh_commands().await;
    }

    /// The shown session's active messages.
    pub(super) async fn transcript(&self) -> Result<Transcript, Error> {
        let params = SessionRef {
            session_id: self.session.clone(),
        };
        self.client
            .call_typed::<SessionTranscript>(&params)
            .await
            .map_err(|e| rpc(&e))
    }

    /// Re-reads the session's commands; on failure the old list stays.
    pub(super) async fn refresh_commands(&mut self) {
        let params = SessionRef {
            session_id: self.session.clone(),
        };
        if let Ok(catalog) = self.client.call_typed::<CommandsCatalog>(&params).await {
            self.commands = catalog.commands;
        }
    }

    /// Whether `name` is a command of the session (an alias counts).
    pub(super) fn registered(&self, name: &str) -> bool {
        self.commands.iter().any(|c| {
            c.disposition != Disposition::Hidden
                && (c.name == name || c.aliases.iter().any(|a| a == name))
        })
    }

    pub(super) fn watermark(&mut self, attached: &Attached) {
        self.epoch.clone_from(&attached.epoch);
        self.last_seq = attached.latest_seq;
    }

    /// Records an event of the shown session as applied; `false` for one
    /// already applied (a replay overlapping the live stream) or of
    /// another session.
    pub(super) fn applies(&mut self, event: &Envelope) -> bool {
        if event.session_id.as_deref() != Some(self.session.as_str()) {
            return false;
        }
        if event.epoch == self.epoch && event.seq <= self.last_seq {
            return false;
        }
        self.epoch.clone_from(&event.epoch);
        self.last_seq = event.seq;
        true
    }

    /// A new connection to `record` re-attached to the shown session from
    /// the watermark: `true` when the daemon replayed what was missed,
    /// `false` when the screen must reload the transcript instead.
    ///
    /// # Errors
    ///
    /// Connection and attachment errors.
    pub(super) async fn reconnect(&mut self, record: DaemonRecord) -> Result<bool, Error> {
        let (client, inbox, _) = connect(&record, &self.profile).await?;
        let params = AttachParams {
            session_id: self.session.clone(),
            role: AttachRole::Driver,
            last_seen_seq: Some(self.last_seq),
            epoch: Some(self.epoch.clone()),
        };
        let attached = client
            .call_typed::<SessionAttach>(&params)
            .await
            .map_err(|e| rpc(&e))?;
        // Owner again: a client that lost its connection lost ownership.
        let _ = resume(&client, &self.session, None).await;
        self.client = client;
        self.inbox = inbox;
        self.record = record;
        if attached.truncated {
            self.watermark(&attached);
        }
        Ok(!attached.truncated)
    }
}

/// The session a reference names.
///
/// # Errors
///
/// `session.not_found`, `session.ambiguous`, `chat.nothing_to_continue`.
pub(super) async fn resolve(
    client: &Client,
    kind: ResolveKind,
    reference: String,
    launch: &Launch,
) -> Result<String, Error> {
    let params = ResolveParams {
        profile: None,
        kind,
        reference,
        cwd: Some(launch.cwd.to_string_lossy().into_owned()),
        terminal: launch.terminal.clone(),
    };
    let found = client
        .call_typed::<SessionResolve>(&params)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(found.session_id)
}

/// Creates a session as its owner (attached by the creation). The
/// idempotency key makes a retry after a lost answer return the same
/// session.
///
/// # Errors
///
/// Creation errors.
pub(super) async fn create(client: &Client, launch: &Launch) -> Result<String, Error> {
    let params = SessionCreateParams {
        profile: None,
        cwd: Some(launch.cwd.to_string_lossy().into_owned()),
        source_tag: None,
        source: Some(ClientKind::Tui),
        idempotency_key: Some(ulid::Ulid::generate().to_string()),
        exclusive: true,
        setup: launch.setup.clone(),
    };
    let created = client
        .call_typed::<SessionCreate>(&params)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(created.session_id)
}

/// Loads `session` in the daemon and attaches as its owner.
///
/// # Errors
///
/// `session.in_use` (75), resume errors.
pub(super) async fn resume(
    client: &Client,
    session: &str,
    setup: Option<&SessionSetup>,
) -> Result<Attached, Error> {
    let params = ResumeParams {
        session_id: session.to_owned(),
        exclusive: true,
        setup: setup.filter(|s| **s != SessionSetup::default()).cloned(),
    };
    client
        .call_typed::<SessionResume>(&params)
        .await
        .map_err(|e| rpc(&e))
}

/// Forks `session` at its last message.
///
/// # Errors
///
/// Fork errors.
pub(super) async fn fork(client: &Client, session: &str) -> Result<String, Error> {
    let params = ForkParams {
        session_id: session.to_owned(),
        at_message_id: None,
        title: None,
    };
    client
        .call_typed::<SessionFork>(&params)
        .await
        .map(|f| f.session_id)
        .map_err(|e| rpc(&e))
}
