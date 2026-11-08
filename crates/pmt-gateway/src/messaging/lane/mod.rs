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
mod automated;
mod branch;
mod clarify;
mod commands;
mod confirm;
mod controls;
mod history;
mod hook_events;
mod input;
mod lifecycle;
mod models;
mod native;
mod observe;
mod pages;
mod private;
mod progress;
mod reply;
mod requests;
mod resume;
mod topic;
mod turns;
mod watchdog;

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
    /// A trajectory event of the session (`session.event`: model output,
    /// tool calls and results, ...).
    Event { kind: String, payload: Value },
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
    /// Text a plugin injected (spec: plugin-system, Plugin live messages
    /// and message injection): a turn of its own after the running one.
    Injected(String),
    /// A confirmation was answered yes: run its action.
    Confirmed {
        action: confirm::Action,
        routing: Box<Routing>,
    },
    /// A message to append to this session once no turn runs: one another
    /// session sent to this chat (`send_message`), or a continuable
    /// scheduled delivery's seed.
    Mirror(pmt_store::NewMessage),
    /// A model was chosen on a picker card: switch to it.
    ModelPicked {
        reference: String,
        routing: Box<Routing>,
    },
    /// An automated turn (goal, loop, heartbeat) for the idle
    /// conversation.
    Automated(crate::messaging::automation::Automated),
    /// A status line to say in the chat.
    Say(String),
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

    /// Whether a person's input waits for the running turn.
    pub(crate) fn human_pending(&self) -> bool {
        self.status.human_pending.load(Ordering::Acquire)
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
    /// Other received files (cached paths), kept by the session.
    files: Vec<String>,
    routing: Routing,
    reply_to: Option<String>,
    /// The user spoke (the reply may be spoken back).
    voice: bool,
    /// When the person sent it (the model may see it).
    sent_at: jiff::Timestamp,
    /// The adapter did not mark it `not_addressed_to_bot`: an intentionally
    /// silent answer still gets a short notice.
    addressed: bool,
}

/// The turn the gateway started in the session.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // independent facts of one running turn
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
    /// The stall watchdog's view of the turn.
    watch: watchdog::Watch,
    /// Tool progress and long-running notices shown in the chat.
    progress: progress::Progress,
    /// The input was addressed to the bot.
    addressed: bool,
    /// The platform's native stream of the answer, if one is used.
    stream: native::Stream,
    /// A heartbeat turn: no typing indicator, tool progress or streamed
    /// draft (approval prompts and failures still show).
    quiet: bool,
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
    mirrors: Vec<pmt_store::NewMessage>,
    /// The event being handled was a voice note.
    voice_input: bool,
    /// The session whose thread already had its title offered (after its
    /// first turn).
    titled: Option<String>,
    /// The command message being handled: what it says quotes it (a
    /// platform answering commands privately, such as Slack's slash
    /// commands, recognises its reply by it).
    answering: Option<String>,
    /// The event being handled's private-answer handle
    /// ([`Inbound::interaction`]).
    interaction: Option<String>,
    /// A turn a restart left `resume_pending` holds the queue until this
    /// deadline (`/retry`, `/discard`, or the expiry release it).
    held: Option<resume::Held>,
    /// `/restart` and `/update` confirmations waiting for an answer.
    confirms: Vec<confirm::Confirm>,
    /// The chat's `/verbose` tool progress mode.
    progress_mode: Option<pmt_config::schema::ChatToolProgress>,
    /// Observed lines were added before the turn being started: it is told
    /// they are context.
    observed_since_turn: bool,
    /// Model picker cards whose buttons still answer.
    pickers: Vec<models::Picker>,
    /// The automation origin of the turn being started (`goal`, `loop`,
    /// `heartbeat`).
    automating: Option<String>,
}

/// Hands an authorized event to its conversation's lane, starting it.
pub(crate) fn input(
    shared: &Arc<Shared>,
    key: &SessionKey,
    routing: Routing,
    inbound: Inbound,
    voice: bool,
) -> Admission {
    let handle = open(shared, key, &routing);
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

/// The lane of `key`, started with `routing` when it has none.
pub(crate) fn open(shared: &Arc<Shared>, key: &SessionKey, routing: &Routing) -> Handle {
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
        titled: None,
        answering: None,
        interaction: None,
        policy: None,
        mirrors: Vec::new(),
        voice_input: false,
        held: None,
        confirms: Vec::new(),
        progress_mode: None,
        observed_since_turn: false,
        pickers: Vec::new(),
        automating: None,
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
            let msg = match self.wake_at() {
                Some(due) => tokio::select! {
                    msg = rx.recv() => msg,
                    () = tokio::time::sleep_until(due) => {
                        self.tick(Instant::now()).await;
                        self.publish_status();
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

    /// The earliest timer of the lane: gathered input, the stall
    /// watchdog, a held turn's expiry.
    fn wake_at(&self) -> Option<Instant> {
        [
            self.buffer.as_ref().map(input::Buffer::due),
            self.stall_due(),
            self.held.as_ref().map(resume::Held::deadline),
            self.confirms.iter().map(confirm::Confirm::deadline).min(),
            self.working_due(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Runs every timer that is due at `now`.
    async fn tick(&mut self, now: Instant) {
        if self.buffer.as_ref().is_some_and(|b| b.due() <= now) {
            self.flush().await;
        }
        self.check_stall(now).await;
        self.check_held(now).await;
        self.expire_confirmations(now);
        self.check_working(now).await;
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
                self.answering = Some(inbound.message_id.clone())
                    .filter(|id| !id.is_empty() && inbound.text.trim_start().starts_with('/'));
                self.interaction.clone_from(&inbound.interaction);
                self.inbound(seq, *routing, *inbound).await;
                self.answering = None;
                self.interaction = None;
            }
            Msg::Finished(finished) => self.finished(finished).await,
            Msg::Delta(text) => {
                self.progressed();
                self.delta(&text).await;
            }
            Msg::Event { kind, payload } => {
                self.hook_event(&kind, &payload).await;
                self.session_event(&kind, &payload).await;
            }
            Msg::Asked { id, method, params } => self.asked(id, &method, &params),
            Msg::Resolved { id, by_client } => self.resolved(&id, &by_client),
            Msg::Cancelled { id, reason } => self.cancelled(&id, &reason),
            Msg::Internal(event) => self.internal(event),
            Msg::Injected(text) => self.injected(text).await,
            Msg::Confirmed { action, routing } => self.confirmed(action, &routing).await,
            Msg::ModelPicked { reference, routing } => {
                self.switch_model(&routing, &reference).await;
            }
            Msg::Automated(automated) => self.automated(automated).await,
            Msg::Say(text) => self.say(&text),
            Msg::Mirror(row) => {
                self.mirrors.push(row);
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
        let new = self.outbound(key, kind, body, edit_of);
        self.shared.deliver(&self.profile, &new);
    }

    /// An outbound message to the conversation's chat.
    fn outbound(
        &self,
        key: String,
        kind: OutboundKind,
        body: String,
        edit_of: Option<String>,
    ) -> NewOutbound {
        NewOutbound {
            idempotency_key: key,
            session_id: self.session.clone(),
            platform: self.routing.platform.clone(),
            chat_id: self.routing.chat_id.clone(),
            thread_id: self.routing.thread_id.clone(),
            reply_to: self.answering.clone(),
            edit_of,
            kind,
            body,
            attachments: Vec::new(),
            notify: None,
        }
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
        for row in self.mirrors.drain(..) {
            crate::messaging::send::append_row(&store, session, &row);
        }
    }

    /// A background event: shown now when idle, after the turn otherwise.
    async fn injected(&mut self, text: String) {
        self.pending.push_back(Pending {
            origin: Origin::Queued,
            text,
            images: Vec::new(),
            files: Vec::new(),
            routing: self.routing.clone(),
            reply_to: None,
            voice: false,
            sent_at: jiff::Timestamp::now(),
            addressed: true,
        });
        if self.running.is_none() {
            self.next().await;
        }
    }

    fn internal(&mut self, event: Background) {
        if self.running.is_some() {
            self.pending.push_back(Pending {
                origin: Origin::Internal,
                text: event.text,
                images: Vec::new(),
                files: Vec::new(),
                routing: self.routing.clone(),
                reply_to: None,
                voice: false,
                sent_at: jiff::Timestamp::now(),
                addressed: true,
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
