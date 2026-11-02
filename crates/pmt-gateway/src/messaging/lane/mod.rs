//! One conversation (spec: messaging-gateway, Single-flight session
//! routing; Busy input policy from configuration only; Queued turns run in
//! FIFO order; Stop preserves the session). Every event of a conversation
//! key is handled by one task, in arrival order, so resolving the key's
//! session, starting turns and queueing input never race:
//!
//! - `input`: text (debounced), media and the busy policy;
//! - `commands`: slash commands (tiers, hooks, busy policy, the gateway's
//!   own commands);
//! - `turns`: the routed session, turns, replies and streaming;
//! - `requests`: approval and clarify prompts and their cards.

mod admin;
mod clarify;
mod commands;
mod input;
mod reply;
mod requests;
mod turns;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use pmt_config::schema::BackgroundNotifications;
use pmt_rpc::contracts::TurnFinished;
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{Admission, Inbound};
use super::key::{Routing, SessionKey};

/// What a lane is told.
#[derive(Debug)]
pub(crate) enum Msg {
    /// An authorized inbound event (`seq`: its admission order).
    Input {
        seq: u64,
        routing: Box<Routing>,
        inbound: Box<Inbound>,
        /// The user spoke (a voice note, now transcribed).
        voice: bool,
    },
    /// A turn of the session ended.
    Finished(TurnFinished),
    /// Streamed answer text.
    Delta(String),
    /// A server request of the session (approval, clarify, ...).
    Asked {
        id: String,
        method: String,
        params: Value,
    },
    /// A request was answered (here or on another surface).
    Resolved { id: String, by_client: String },
    /// A request settled unanswered.
    Cancelled { id: String, reason: String },
    /// A background notification.
    Internal(Background),
    /// A message another session sent to this chat (`send_message`), to
    /// mirror into this session once no turn runs.
    Mirror {
        text: String,
        from_session: Option<String>,
    },
}

/// What a lane shows outside its task.
#[derive(Debug, Default)]
pub(crate) struct Status {
    running: AtomicBool,
    /// A human follow-up waits for the running turn.
    human_pending: AtomicBool,
    /// Admission counter.
    seq: AtomicU64,
}

/// A lane's mailbox.
#[derive(Debug, Clone)]
pub(crate) struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
    status: Arc<Status>,
}

impl Handle {
    pub(crate) fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// Whether the conversation runs a turn.
    pub(crate) fn running(&self) -> bool {
        self.status.running.load(Ordering::Acquire)
    }
}

/// A background process or subagent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Background {
    /// What happened.
    pub kind: BackgroundKind,
    /// The notification text.
    pub text: String,
}

/// Kinds of background events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundKind {
    /// Progress output.
    Progress,
    /// A successful result (exit 0, subagent done).
    Result,
    /// A failure.
    Error,
}

impl BackgroundKind {
    /// Whether `level` lets this kind through.
    #[must_use]
    pub fn shown_at(self, level: BackgroundNotifications) -> bool {
        match level {
            BackgroundNotifications::All => true,
            BackgroundNotifications::Concise => self != Self::Progress,
            BackgroundNotifications::Result => self == Self::Result,
            BackgroundNotifications::Error => self == Self::Error,
            BackgroundNotifications::Off => false,
        }
    }
}

/// Where queued input came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Follow-ups merged while a turn ran.
    Human,
    /// One `/queue` entry (its own turn).
    Queued,
    /// A command whose busy policy waits for the turn.
    Command,
    /// A background notification.
    Internal,
}

/// Input waiting for the running turn.
#[derive(Debug, Clone)]
struct Pending {
    origin: Origin,
    text: String,
    images: Vec<String>,
    routing: Routing,
    reply_to: Option<String>,
    /// The user spoke (the reply may be spoken back).
    voice: bool,
}

/// The turn the gateway started in the session.
#[derive(Debug)]
struct Running {
    turn_id: String,
    /// Who asked (may answer its approvals).
    requester: String,
    reply_to: Option<String>,
    /// `/stop` stopped it: nothing is delivered.
    stopped: bool,
    draft: turns::Draft,
    /// The user spoke.
    voice: bool,
    /// Keeps the typing indicator alive while the turn runs.
    _typing: Option<tokio_util::sync::DropGuard>,
    /// The reply's audio streamed to the platform, sentence by sentence.
    speech: Option<reply::Spoken>,
}

/// A transition run once the stopped turn has ended.
#[derive(Debug, Clone)]
enum Deferred {
    /// `/new` (`reset`: typed as `/reset`).
    New { reset: bool, routing: Routing },
}

struct Lane {
    shared: Arc<Shared>,
    key: SessionKey,
    profile: String,
    status: Arc<Status>,
    tx: mpsc::WeakUnboundedSender<Msg>,
    /// The latest routing of the conversation (where replies go).
    routing: Routing,
    session: Option<String>,
    running: Option<Running>,
    pending: VecDeque<Pending>,
    buffer: Option<input::Buffer>,
    last_ack: Option<Instant>,
    requests: requests::Open,
    deferred: Option<Deferred>,
    /// Admissions up to here happened before the latest session
    /// transition completed: a `/new` among them shares its result.
    horizon: u64,
    /// The chat policy frozen for the current session.
    policy: Option<pmt_config::schema::ChatPolicy>,
    /// Mirrored messages waiting for the running turn to end.
    mirrors: Vec<(String, Option<String>)>,
    /// The event being handled was a voice note.
    voice_input: bool,
}

/// Hands an authorized event to its conversation's lane, starting it.
pub(crate) fn input(
    shared: &Arc<Shared>,
    key: &SessionKey,
    routing: Routing,
    inbound: Inbound,
    voice: bool,
) -> Admission {
    let handle = lane(shared, key, &routing);
    let seq = handle.status.seq.fetch_add(1, Ordering::AcqRel) + 1;
    let queued = handle.status.running.load(Ordering::Acquire);
    handle.send(Msg::Input {
        seq,
        routing: Box::new(routing),
        inbound: Box::new(inbound),
        voice,
    });
    if queued {
        Admission::Queued
    } else {
        Admission::Accepted
    }
}

fn lane(shared: &Arc<Shared>, key: &SessionKey, routing: &Routing) -> Handle {
    let mut lanes = shared.lanes.lock().expect("lanes poisoned");
    if let Some(handle) = lanes.get(key.as_str()) {
        return handle.clone();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let status = Arc::new(Status::default());
    let handle = Handle {
        tx: tx.clone(),
        status: Arc::clone(&status),
    };
    let lane = Lane {
        shared: Arc::clone(shared),
        key: key.clone(),
        profile: routing.profile.clone(),
        status,
        tx: tx.downgrade(),
        routing: routing.clone(),
        session: None,
        running: None,
        pending: VecDeque::new(),
        buffer: None,
        last_ack: None,
        requests: requests::Open::default(),
        deferred: None,
        horizon: 0,
        policy: None,
        mirrors: Vec::new(),
        voice_input: false,
    };
    let span = tracing::info_span!(
        "conversation",
        profile = routing.profile,
        platform = routing.platform
    );
    tokio::spawn(lane.run(rx).instrument(span));
    lanes.insert(key.as_str().to_owned(), handle.clone());
    handle
}

/// A background event for the conversation of `session_id`: filtered by
/// `gateway.notifications.background`, delivered after the running turn,
/// and refused (`dropped`, retried later by the caller) while a human
/// follow-up waits.
pub(crate) fn notify(
    shared: &Shared,
    profile: &str,
    session_id: &str,
    event: Background,
) -> Admission {
    let level = shared
        .agents
        .scope(profile)
        .map(|s| s.config().gateway.notifications.background)
        .unwrap_or_default();
    if !event.kind.shown_at(level) {
        return Admission::Accepted;
    }
    let key = shared
        .sessions
        .lock()
        .expect("sessions poisoned")
        .get(session_id)
        .cloned();
    let handle = key.and_then(|k| {
        shared
            .lanes
            .lock()
            .expect("lanes poisoned")
            .get(&k)
            .cloned()
    });
    let Some(handle) = handle else {
        return Admission::Dropped;
    };
    if handle.status.human_pending.load(Ordering::Acquire) {
        return Admission::Dropped;
    }
    let running = handle.status.running.load(Ordering::Acquire);
    handle.send(Msg::Internal(event));
    if running {
        Admission::Queued
    } else {
        Admission::Accepted
    }
}

/// Waits until no conversation runs a turn, at most `deadline`; `true`
/// when none does.
pub(crate) async fn drain(shared: &Shared, deadline: Duration) -> bool {
    let until = Instant::now() + deadline;
    loop {
        let busy = shared
            .lanes
            .lock()
            .expect("lanes poisoned")
            .values()
            .any(|h| h.status.running.load(Ordering::Acquire));
        if !busy {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

impl Lane {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        loop {
            let msg = match self.buffer.as_ref().map(input::Buffer::due) {
                Some(due) => tokio::select! {
                    msg = rx.recv() => msg,
                    () = tokio::time::sleep_until(due) => {
                        self.flush().await;
                        continue;
                    }
                },
                None => rx.recv().await,
            };
            let Some(msg) = msg else { return };
            self.handle(msg).await;
            self.publish_status();
        }
    }

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input {
                seq,
                routing,
                inbound,
                voice,
            } => {
                self.voice_input = voice;
                self.inbound(seq, *routing, *inbound).await;
            }
            Msg::Finished(finished) => self.finished(finished).await,
            Msg::Delta(text) => self.delta(&text).await,
            Msg::Asked { id, method, params } => self.asked(id, &method, &params),
            Msg::Resolved { id, by_client } => self.resolved(&id, &by_client),
            Msg::Cancelled { id, reason } => self.cancelled(&id, &reason),
            Msg::Internal(event) => self.internal(event),
            Msg::Mirror { text, from_session } => {
                self.mirrors.push((text, from_session));
                if self.running.is_none() {
                    self.flush_mirrors();
                }
            }
        }
    }

    fn publish_status(&self) {
        self.status
            .running
            .store(self.running.is_some(), Ordering::Release);
        let human = self.buffer.is_some()
            || self
                .pending
                .iter()
                .any(|p| matches!(p.origin, Origin::Human | Origin::Queued));
        self.status.human_pending.store(human, Ordering::Release);
    }

    /// Sends a gateway message to the conversation's chat.
    fn say(&self, text: &str) {
        self.say_as(OutboundKind::System, text, None);
    }

    fn say_as(&self, kind: OutboundKind, text: &str, edit_of: Option<String>) {
        let key = format!("{}:{}", kind.as_str(), crate::tickets::random_hex(8));
        self.deliver(key, kind, text.to_owned(), edit_of);
    }

    fn deliver(&self, key: String, kind: OutboundKind, body: String, edit_of: Option<String>) {
        let new = NewOutbound {
            idempotency_key: key,
            session_id: self.session.clone(),
            platform: self.routing.platform.clone(),
            chat_id: self.routing.chat_id.clone(),
            thread_id: self.routing.thread_id.clone(),
            reply_to: None,
            edit_of,
            kind,
            body,
            attachments: Vec::new(),
        };
        self.shared.deliver(&self.profile, &new);
    }

    /// The profile's gateway configuration now.
    fn config(&self) -> Option<pmt_config::ProfileScope> {
        self.shared.agents.scope(&self.profile)
    }

    /// Appends the waiting mirrored messages to the session (never while
    /// a turn runs: it owns the transcript then).
    fn flush_mirrors(&mut self) {
        let (Some(store), Some(session)) = (
            self.shared.store(&self.profile),
            self.session.as_deref().and_then(|s| s.parse().ok()),
        ) else {
            self.mirrors.clear();
            return;
        };
        for (text, from) in self.mirrors.drain(..) {
            crate::messaging::send::append_mirror(&store, session, &text, from.as_deref());
        }
    }

    /// A background event: shown now when idle, after the turn otherwise.
    fn internal(&mut self, event: Background) {
        if self.running.is_some() {
            self.pending.push_back(Pending {
                origin: Origin::Internal,
                text: event.text,
                images: Vec::new(),
                routing: self.routing.clone(),
                reply_to: None,
                voice: false,
            });
        } else {
            self.say_as(OutboundKind::Notification, &event.text, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_only_hides_a_successful_exit() {
        assert!(!BackgroundKind::Result.shown_at(BackgroundNotifications::Error));
        assert!(BackgroundKind::Error.shown_at(BackgroundNotifications::Error));
        assert!(BackgroundKind::Result.shown_at(BackgroundNotifications::Concise));
        assert!(!BackgroundKind::Progress.shown_at(BackgroundNotifications::Concise));
        assert!(!BackgroundKind::Error.shown_at(BackgroundNotifications::Off));
    }
}
