//! The TUI's side of the client protocol (spec: tui, Hello handshake is
//! mandatory and versioned; Session access requires an attachment; Event
//! sequencing and reconnect replay; Idempotent session creation; Profile
//! scope on every request): the `tui` handshake, opening the session as
//! its interactive owner (created with an idempotency key, or the one
//! `-r`/`-c` names), the event watermark, and re-attaching after a lost
//! connection with that watermark, reloading the transcript when the
//! daemon says the replay is `truncated` (another epoch, or evicted).
//!
//! A TUI may hold several live sessions (spec: tui, Live session
//! switcher): it stays attached to each as owner, keeps a watermark per
//! session and shows one; the others keep running in the background.

use std::collections::HashMap;

use pmt_core::error::Error;
use pmt_rpc::client::{Client, Inbox, error_of};
use pmt_rpc::contracts::{
    AttachParams, AttachRole, Attached, ClientKind, CommandEntry, CommandsCatalog, Disposition,
    Envelope, ForkParams, ResolveKind, ResolveParams, ResumeParams, SessionAttach, SessionCreate,
    SessionCreateParams, SessionDetach, SessionFork, SessionRef, SessionResolve, SessionResume,
    SessionSetup, SessionTranscript, Transcript,
};
use pmt_rpc::discovery::DaemonRecord;

use super::Launch;

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
    /// Every session this TUI is attached to, in the order it opened them.
    pub(super) live: Vec<String>,
    /// The daemon epoch of the watermarks.
    pub(super) epoch: String,
    /// The newest event sequence number applied, per live session.
    pub(super) seen: HashMap<String, u64>,
    /// The session's commands (`commands.catalog`).
    pub(super) commands: Vec<CommandEntry>,
    /// The profile the ticket grants.
    pub(super) profile: String,
    /// The daemon.
    pub(super) record: DaemonRecord,
}

/// Connects to `record`'s daemon for `profile` as a `tui` client (it
/// answers server requests); also returns the stale-build notice.
///
/// # Errors
///
/// `rpc.ticket`, `rpc.connection`, `rpc.protocol_mismatch` (76).
pub(super) async fn connect(
    record: &DaemonRecord,
    profile: &str,
) -> Result<(Client, Inbox, Option<String>), Error> {
    let hello = crate::daemon_client::hello(ClientKind::Tui, true);
    crate::daemon_client::connect(record, profile, &hello).await
}

impl Link {
    /// Connects and opens the session `launch` names (or a new one).
    ///
    /// # Errors
    ///
    /// Connection errors, `session.in_use` (75), resolution and creation
    /// errors.
    /// `recent` is the session to re-attach to (`ui.tui.auto_resume`);
    /// when it cannot be resumed (deleted, in use) a new one is opened.
    pub(super) async fn open(
        record: DaemonRecord,
        profile: &str,
        launch: &Launch,
        recent: Option<String>,
    ) -> Result<(Self, Option<String>), Error> {
        let (client, inbox, notice) = connect(&record, profile).await?;
        let epoch = client.hello().epoch.clone();
        let mut link = Self {
            client,
            inbox,
            session: String::new(),
            live: Vec::new(),
            epoch,
            seen: HashMap::new(),
            commands: Vec::new(),
            profile: profile.to_owned(),
            record,
        };
        // Resolved first: the store, not the daemon's memory, says whether
        // the session still exists.
        if let Some(recent) = recent
            && let Ok(found) = resolve(&link.client, ResolveKind::Resume, recent, launch).await
            && let Ok(attached) = resume(&link.client, &found, None).await
        {
            link.adopt(found, &attached);
            link.refresh_commands().await;
            return Ok((link, notice));
        }
        // A new session is loaded at once too, so its commands (skills
        // included) list.
        let (session, setup) = if let Some(found) = link.target(launch).await? {
            (found, Some(&launch.setup))
        } else {
            (create(&link.client, launch).await?, None)
        };
        let attached = resume(&link.client, &session, setup).await?;
        link.adopt(session, &attached);
        link.refresh_commands().await;
        Ok((link, notice))
    }

    /// Shows `session` (attached as `attached` says), keeping the others
    /// live.
    fn adopt(&mut self, session: String, attached: &Attached) {
        self.epoch.clone_from(&attached.epoch);
        self.seen.insert(session.clone(), attached.latest_seq);
        if !self.live.contains(&session) {
            self.live.push(session.clone());
        }
        self.session = session;
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

    /// Shows `session` (attached as `attached` says); the session shown
    /// before stays live.
    pub(super) async fn switch(&mut self, session: String, attached: &Attached) {
        self.adopt(session, attached);
        self.refresh_commands().await;
    }

    /// Shows the live session `session` (no round-trip but its commands).
    pub(super) async fn show(&mut self, session: &str) {
        if self.live.iter().any(|s| s == session) {
            session.clone_into(&mut self.session);
            self.refresh_commands().await;
        }
    }

    /// Detaches from the live session `session` and forgets it; the
    /// daemon closes it once idle. Another live session is shown when it
    /// was the shown one.
    pub(super) async fn close(&mut self, session: &str) {
        let params = SessionRef {
            session_id: session.to_owned(),
        };
        let _ = self.client.call_typed::<SessionDetach>(&params).await;
        self.live.retain(|s| s != session);
        self.seen.remove(session);
        if self.session == session
            && let Some(next) = self.live.last().cloned()
        {
            self.show(&next).await;
        }
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

    /// Records an event of a live session as applied; `false` for one
    /// already applied (a replay overlapping the live stream) or of a
    /// session this TUI does not hold.
    pub(super) fn applies(&mut self, event: &Envelope) -> bool {
        let Some(session) = event.session_id.as_deref() else {
            return false;
        };
        let Some(seen) = self.seen.get_mut(session) else {
            return false;
        };
        if event.epoch == self.epoch && event.seq <= *seen {
            return false;
        }
        *seen = event.seq;
        self.epoch.clone_from(&event.epoch);
        true
    }

    /// A new connection to `record` re-attached to every live session from
    /// its watermark: `true` when the daemon replayed what the shown one
    /// missed, `false` when the screen must reload its transcript instead.
    /// A live session that cannot be re-attached (deleted meanwhile) is
    /// dropped; the shown one failing is an error.
    ///
    /// # Errors
    ///
    /// Connection and attachment errors.
    pub(super) async fn reconnect(&mut self, record: DaemonRecord) -> Result<bool, Error> {
        let (client, inbox, _) = connect(&record, &self.profile).await?;
        let mut replayed = true;
        for session in self.live.clone() {
            let params = AttachParams {
                session_id: session.clone(),
                role: AttachRole::Driver,
                last_seen_seq: self.seen.get(&session).copied(),
                epoch: Some(self.epoch.clone()),
            };
            let attached = match client.call_typed::<SessionAttach>(&params).await {
                Ok(attached) => attached,
                Err(err) if session == self.session => return Err(rpc(&err)),
                Err(_) => {
                    self.live.retain(|s| *s != session);
                    self.seen.remove(&session);
                    continue;
                }
            };
            // Owner again: a client that lost its connection lost ownership.
            let _ = resume(&client, &session, None).await;
            if attached.truncated {
                self.epoch.clone_from(&attached.epoch);
                self.seen.insert(session.clone(), attached.latest_seq);
                replayed &= session != self.session;
            }
        }
        self.client = client;
        self.inbox = inbox;
        self.record = record;
        Ok(replayed)
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
