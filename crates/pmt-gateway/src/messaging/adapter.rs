//! The host side of a messaging adapter (spec: platform-adapters, Adapter
//! contract; Typed admission result; Adapter state reporting and
//! reconnect): what an adapter declares, what it submits and what it is
//! asked to send. Adapters stay thin: splitting, retries, authorization,
//! routing and delivery bookkeeping are the gateway's.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_config::schema::PlatformConfig;
use pmt_core::error::Error;
pub use pmt_store::gateway::outbox::OutboundAttachment;
use serde::Serialize;

use super::key::ChatType;

/// A boxed adapter future.
pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub use super::descriptor::{
    AccessPolicy, ContinuableSurface, Descriptor, Interactive, LengthUnit, Markup, MediaKind,
};

/// One inbound attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// What it is.
    pub kind: MediaKind,
    /// Where the gateway can read it: a platform reference the adapter
    /// opens with [`Adapter::open_media`], or (once cached) a local path.
    pub location: String,
    /// The file name, when the platform gives one.
    pub name: Option<String>,
    /// Length of a voice or audio clip.
    pub duration_secs: Option<u64>,
}

impl Attachment {
    /// An attachment of `kind` at `location`.
    #[must_use]
    pub fn new(kind: MediaKind, location: &str) -> Self {
        Self {
            kind,
            location: location.to_owned(),
            name: None,
            duration_secs: None,
        }
    }
}

/// One inbound event, normalized by the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// The platform's message id.
    pub message_id: String,
    /// Canonical sender id.
    pub sender_id: String,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// Chat id.
    pub chat_id: String,
    /// Chat kind.
    pub chat_type: ChatType,
    /// Workspace or server id.
    pub scope_id: Option<String>,
    /// Thread id.
    pub thread_id: Option<String>,
    /// When it was sent.
    pub timestamp: Timestamp,
    /// The text.
    pub text: String,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// The message it answers.
    pub reply_to: Option<String>,
    /// Sent by a bot.
    pub is_bot: bool,
    /// A pressed button's data (an interactive card's choice).
    pub callback: Option<String>,
    /// The platform's handle for answering this event privately (a
    /// Telegram callback query id, a Discord interaction), handed back in
    /// [`Private::interaction`].
    pub interaction: Option<String>,
    /// The profile a multiplexing connector (the relay) addresses; `None`
    /// is the adapter's own profile.
    pub profile: Option<String>,
    /// How it addresses the bot.
    pub addressing: Addressing,
}

/// How an inbound message addresses the bot, as the adapter sees it (spec:
/// messaging-gateway, Group mention gating and observe mode; Bot-authored
/// message admission; Intentional silence tokens).
#[allow(clippy::struct_excessive_bools)] // independent facts about one message
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Addressing {
    /// It carries a literal inline mention of the bot (`@bot`, `<@id>`).
    pub mentioned: bool,
    /// It replies to one of the bot's messages.
    pub replies_to_bot: bool,
    /// It is in a thread the bot already takes part in.
    pub in_bot_thread: bool,
    /// The adapter marks it `not_addressed_to_bot` (chatter the bot may
    /// read without being asked, such as a free-response channel).
    pub not_addressed: bool,
}

impl Inbound {
    /// A text message from `sender` in `chat`.
    #[must_use]
    pub fn text(chat_type: ChatType, chat: &str, sender: &str, text: &str) -> Self {
        Self {
            message_id: String::new(),
            sender_id: sender.to_owned(),
            sender_name: None,
            chat_id: chat.to_owned(),
            chat_type,
            scope_id: None,
            thread_id: None,
            timestamp: Timestamp::now(),
            text: text.to_owned(),
            attachments: Vec::new(),
            reply_to: None,
            is_bot: false,
            callback: None,
            interaction: None,
            profile: None,
            addressing: Addressing::default(),
        }
    }
}

/// A reply only one member of a chat may see (a refusal to a bystander).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Private {
    /// The chat the member wrote in.
    pub chat_id: String,
    /// Its thread, when inside one.
    pub thread_id: Option<String>,
    /// Who sees it.
    pub sender_id: String,
    /// The event's [`Inbound::interaction`], when it had one.
    pub interaction: Option<String>,
    /// What they are told.
    pub text: String,
}

/// A button on an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Button {
    /// What it shows.
    pub label: String,
    /// What a press submits as [`Inbound::callback`].
    pub data: String,
}

/// A multi-select control on an outgoing message (Discord select menus,
/// Slack checkboxes with a submit button, `WhatsApp` multi-select polls).
/// A submission arrives as [`Inbound::callback`] `"<data>|<numbers>"`,
/// the 1-based numbers of the picked options separated by commas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Select {
    /// The signed data the submission carries back.
    pub data: String,
    /// The options, in order.
    pub options: Vec<String>,
}

impl Select {
    /// The callback a submission of `picked` (1-based numbers) carries.
    #[must_use]
    pub fn submission(data: &str, picked: &[usize]) -> String {
        let numbers: Vec<String> = picked.iter().map(ToString::to_string).collect();
        format!("{data}|{}", numbers.join(","))
    }
}

/// One message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// The message it answers.
    pub reply_to: Option<String>,
    /// The text.
    pub text: String,
    /// Buttons (adapters with `interactive = native`).
    pub buttons: Vec<Button>,
    /// A multi-select control (adapters with `interactive = native`; the
    /// text numbers the options for the others).
    pub select: Option<Select>,
    /// Files sent with it (from `MEDIA:` directives).
    pub attachments: Vec<OutboundAttachment>,
    /// Send without markup (the resend after a `formatting` refusal).
    pub plain: bool,
    /// How it notifies the recipient, for text and media alike: `None`
    /// leaves it to the adapter (its quiet notification mode, where it has
    /// one), `Some(true)` is a final, notifying message whatever that mode
    /// says, `Some(false)` is sent silently where the platform can.
    pub notify: Option<bool>,
}

impl Outgoing {
    /// Plain text to a chat (and thread).
    #[must_use]
    pub fn text(chat_id: &str, thread_id: Option<&str>, text: &str) -> Self {
        Self {
            chat_id: chat_id.to_owned(),
            thread_id: thread_id.map(str::to_owned),
            reply_to: None,
            text: text.to_owned(),
            buttons: Vec::new(),
            select: None,
            attachments: Vec::new(),
            plain: false,
            notify: None,
        }
    }
}

/// A successful send.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sent {
    /// The platform's id of the message, when it has one.
    pub message_id: Option<String>,
}

/// Why a send failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The platform throttles; retry after the hint.
    RateLimited {
        /// The platform's `retry_after`.
        retry_after: Option<Duration>,
    },
    /// The platform could not be reached.
    Network(String),
    /// The platform failed (5xx).
    Server(String),
    /// The request was transmitted but its outcome is unknown.
    Ambiguous(String),
    /// The platform refused the message for good.
    Permanent(String),
    /// The platform refused the message's markup (resent once as plain
    /// text).
    Formatting(String),
    /// The operation is not supported.
    Unsupported,
}

impl SendError {
    /// A description for the outbox entry.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::RateLimited { retry_after } => match retry_after {
                Some(after) => format!("rate limited (retry after {}s)", after.as_secs()),
                None => "rate limited".to_owned(),
            },
            Self::Network(e) => format!("network: {e}"),
            Self::Server(e) => format!("server error: {e}"),
            Self::Ambiguous(e) => format!("outcome unknown: {e}"),
            Self::Permanent(e) => format!("refused: {e}"),
            Self::Formatting(e) => format!("markup refused: {e}"),
            Self::Unsupported => "unsupported".to_owned(),
        }
    }
}

/// Why an adapter could not connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    /// Reconnecting may help.
    pub retryable: bool,
    /// A short code (`missing_secret`, `telegram_lock`, `unauthorized`).
    pub code: String,
    /// What happened.
    pub message: String,
}

/// An adapter's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterState {
    /// Not enabled.
    Disabled,
    /// Connecting.
    Connecting,
    /// Connected.
    Connected,
    /// Connected, send path not yet confirmed.
    Degraded,
    /// Waiting to reconnect.
    Retrying,
    /// Stopped for good until reconfigured.
    Fatal,
}

/// One adapter's state, for `gateway status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdapterStatus {
    /// The owning profile.
    pub profile: String,
    /// Platform name.
    pub platform: String,
    /// Its state.
    pub state: AdapterState,
    /// Error code for `degraded`, `retrying` and `fatal`.
    pub code: Option<String>,
    /// Error message.
    pub message: Option<String>,
    /// Outside `connected` for longer than `gateway.reconnect_attention_secs`.
    #[serde(default)]
    pub attention: bool,
    /// An admin paused it (`/platform pause`): inbound messages are
    /// acknowledged and dropped, outbound delivery continues.
    #[serde(default)]
    pub paused: bool,
    /// What the adapter declared (as loaded: the limit lowered by the
    /// platform table); `None` when it could not be built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<Descriptor>,
}

/// What submitting an inbound event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// It starts or joins a turn.
    Accepted,
    /// Already admitted.
    Duplicate,
    /// The sender may not use the bot.
    Unauthorized,
    /// The sender got a pairing code.
    PairingIssued,
    /// Over the sender's rate limit.
    RateLimited,
    /// Queued behind the running turn.
    Queued,
    /// Not routed (unserved profile, refused bot message).
    Dropped,
    /// The adapter is paused by an admin: acknowledged and dropped.
    Paused,
    /// Taken without starting a turn (a start ping, an observed group
    /// message).
    Handled,
}

/// An adapter's verdict on a sender (spec: platform-adapters, Adapter
/// access-policy delegation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderVerdict {
    /// The adapter's own allowlist admits the sender.
    Allow,
    /// The adapter refuses the sender.
    Deny,
    /// No verdict: the gateway's own rules decide (and deny by default).
    Unknown,
}

impl SenderVerdict {
    /// The verdict an answer in words stands for (plugin adapters answer
    /// across the plugin boundary): anything but `allow` or `deny` is
    /// `unknown`.
    #[must_use]
    pub fn from_answer(answer: &str) -> Self {
        match answer {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::Unknown,
        }
    }

    /// The verdict of a check that may have failed: a failure is
    /// `unknown`.
    #[must_use]
    pub fn of<E>(result: Result<Self, E>) -> Self {
        result.unwrap_or(Self::Unknown)
    }
}

/// What firing a scheduled job from an adapter did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronFire {
    /// A run was queued.
    Queued {
        /// The job's id.
        job_id: String,
        /// The run's id.
        run_id: String,
    },
    /// Dropped: the job is paused or already running.
    Dropped {
        /// Why.
        reason: String,
    },
}

/// The gateway, as an adapter reaches it.
pub trait AdapterLink: Send + Sync {
    /// Submits an inbound event.
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission>;
    /// Reports a state change.
    fn report(&self, state: AdapterState, error: Option<AdapterError>);
    /// Sends `text` to a send target of the adapter's profile
    /// (`platform[:chat_id[:thread_id]]`) without an agent turn (webhook
    /// direct delivery); the resolved destination's label, or why it could
    /// not be queued.
    fn deliver<'a>(
        &'a self,
        _target: &'a str,
        _text: &'a str,
    ) -> AdapterFuture<'a, Result<String, String>> {
        Box::pin(async { Err("delivery is not available".to_owned()) })
    }
    /// Appends `text` to the session `target`'s chat routes to, as an
    /// attributed assistant message (a webhook route's
    /// `mirror_to_session`); nothing when that chat has no session.
    fn mirror<'a>(&'a self, _target: &'a str, _text: &'a str) -> AdapterFuture<'a, ()> {
        Box::pin(async {})
    }
    /// Whether the adapter's profile has the scheduled job `job` (an id or
    /// name): its id, or why not (a webhook route's `cron_job`, checked
    /// when the route loads).
    fn find_cron_job<'a>(&'a self, _job: &'a str) -> AdapterFuture<'a, Result<String, String>> {
        Box::pin(async { Err("scheduled jobs are not available".to_owned()) })
    }
    /// Fires scheduled job `job` of the adapter's profile once, with
    /// `context` as the run's transient `## Run Context`, through the same
    /// at-most-once claim as a manual run; a paused job or one already
    /// running is dropped.
    fn fire_cron_job<'a>(
        &'a self,
        _job: &'a str,
        _context: &'a str,
    ) -> AdapterFuture<'a, Result<CronFire, String>> {
        Box::pin(async { Err("scheduled jobs are not available".to_owned()) })
    }
    /// Whether `sender_id` may use the bot in a direct message (inline
    /// pickers answer nothing to anyone else).
    fn authorized<'a>(&'a self, _sender_id: &'a str) -> AdapterFuture<'a, bool> {
        Box::pin(async { false })
    }
    /// The address the shared HTTP listener is (or will be) bound to.
    /// Fixed for the daemon's lifetime: a later edit of `daemon.toml` does
    /// not move the listener, so security checks read this, not the file.
    /// `None`: no listener (unsigned routes are then refused).
    fn http_listen(&self) -> Option<std::net::SocketAddr> {
        None
    }
}

/// One entry of a native command menu: a gateway command or a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuCommand {
    /// Name without `/`.
    pub name: String,
    /// One line describing it.
    pub description: String,
    /// It takes arguments.
    pub takes_args: bool,
}

/// What an adapter asks of the sessions it starts in one chat or thread
/// (a Telegram topic's skill, a webhook route's skills and toolsets).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHints {
    /// Skills preloaded into each new session.
    pub skills: Vec<String>,
    /// Toolsets replacing the session's defaults.
    pub toolsets: Option<Vec<String>>,
}

impl SessionHints {
    /// Whether it asks for nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.toolsets.is_none()
    }
}

/// A messaging adapter.
pub trait Adapter: Send + Sync + 'static {
    /// What it supports; read before `connect`.
    fn describe(&self) -> Descriptor;
    /// Connects; inbound events go to `link` until `disconnect`.
    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>>;
    /// Disconnects.
    fn disconnect(&self) -> AdapterFuture<'_, ()>;
    /// Sends one message.
    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>>;
    /// Replaces a sent message's text (adapters with `supports_edit`).
    fn edit<'a>(
        &'a self,
        _message_id: &'a str,
        _message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Deletes a sent message (adapters with `supports_delete`); a message
    /// sent into a thread names it.
    fn delete<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
        _message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Adds (`remove = false`) or removes a reaction on a message
    /// (adapters with `supports_reactions`).
    fn react<'a>(&'a self, _reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// The chat id of a channel known by name (`platform:#name` targets);
    /// `None` when the platform has no such directory.
    fn resolve_channel<'a>(&'a self, _name: &'a str) -> AdapterFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// A chat's name and kind.
    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>>;
    /// Shows the typing indicator once (adapters with `supports_typing`).
    fn send_typing<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// What identifies the platform credential this adapter connects with
    /// (a bot token), so two homes or profiles never connect it twice;
    /// `None` when the credential is not unique to one connection.
    fn credential_identity(&self) -> Option<String> {
        None
    }
    /// Opens an inbound attachment for download.
    fn open_media<'a>(
        &'a self,
        _attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Plays streamed reply audio (adapters with `streaming_tts`).
    fn audio<'a>(&'a self, _op: &'a AudioOp) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Whether the adapter's own access policy admits the event's sender;
    /// asked only of adapters declaring `enforces_own_access_policy =
    /// allowlist`, and only when no gateway rule admitted the sender.
    /// Failing, or answering `unknown`, leaves the gateway's default deny.
    fn authorize_sender<'a>(
        &'a self,
        _inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<SenderVerdict, AdapterError>> {
        Box::pin(async { Ok(SenderVerdict::Unknown) })
    }
    /// The role ids the event's sender holds in its chat's workspace
    /// (adapters declaring `exposes_roles`: Discord roles, Slack user
    /// groups, Matrix power levels); an error denies a role grant.
    fn member_roles<'a>(
        &'a self,
        _inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<Vec<String>, SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// The id of the role named `name` (case-insensitive) in the
    /// workspaces the bot is in, when the adapter can tell.
    fn resolve_role<'a>(&'a self, _name: &'a str) -> AdapterFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
    /// The chat's (or thread's) latest messages, oldest first, at most
    /// `limit` (adapters declaring `can_fetch_history`).
    fn fetch_history<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
        _limit: usize,
    ) -> AdapterFuture<'a, Result<Vec<HistoryEntry>, SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Whether an HTTP request on the shared listener carries the
    /// platform's valid signature or shared secret (adapters with
    /// `binds_http`); nothing unverified is handled.
    fn verify_http(&self, _request: &HttpRequest) -> bool {
        false
    }
    /// Handles a verified request on the shared listener.
    fn handle_http<'a>(&'a self, _request: &'a HttpRequest) -> AdapterFuture<'a, HttpResponse> {
        Box::pin(async { HttpResponse::status(404) })
    }
    /// The largest request body the adapter reads on the shared listener;
    /// a larger one is answered 413 before it is read.
    fn max_body_bytes(&self) -> usize {
        DEFAULT_MAX_BODY
    }
    /// Registers `commands` as the platform's native command menu
    /// (adapters with one); called after each connect and after a
    /// registry change. An adapter configured as a follower registers and
    /// removes nothing.
    fn register_commands<'a>(
        &'a self,
        _commands: &'a [MenuCommand],
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Ok(()) })
    }
    /// The session of `thread_id` got its title after its first turn (a
    /// thread the adapter opened may take it as its name).
    fn thread_titled<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: &'a str,
        _title: &'a str,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async {})
    }
    /// What new sessions in `chat_id` / `thread_id` start with.
    fn session_hints(&self, _chat_id: &str, _thread_id: Option<&str>) -> SessionHints {
        SessionHints::default()
    }
    /// Opens a new thread (or topic) titled `title` in `chat_id`; its id.
    /// `Unsupported` where the platform (or this chat) has no threads.
    fn open_thread<'a>(
        &'a self,
        _chat_id: &'a str,
        _title: &'a str,
    ) -> AdapterFuture<'a, Result<String, SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Starts a native stream (adapters declaring `native_stream`) where
    /// `message` would go; the id of the message it streams into.
    fn stream_start<'a>(
        &'a self,
        _message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<String, SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Appends `text` to stream `message_id` of `chat_id`.
    fn stream_append<'a>(
        &'a self,
        _chat_id: &'a str,
        _message_id: &'a str,
        _text: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Shows or updates `task` as a native task card in stream
    /// `message_id` (adapters declaring `task_cards`).
    fn stream_task<'a>(
        &'a self,
        _chat_id: &'a str,
        _message_id: &'a str,
        _task: &'a TaskCard,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Ends stream `message_id` (it is an ordinary message from then on).
    fn stream_stop<'a>(
        &'a self,
        _chat_id: &'a str,
        _message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Shows `status` ("is thinking…") as the working status of the chat
    /// or thread, or clears it with `None` (adapters declaring
    /// `status_line`).
    fn set_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
        _status: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Tells `to.sender_id` alone `to.text`: ephemerally in the chat where
    /// the platform can, else in a direct message (adapters declaring
    /// `private_replies`). The gateway posts in the chat only when this
    /// fails.
    fn reply_privately<'a>(&'a self, _to: &'a Private) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
    /// Renames thread (or topic) `thread_id` of `chat_id` to `title`.
    fn rename_thread<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: &'a str,
        _title: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async { Err(SendError::Unsupported) })
    }
}

/// The body limit of the shared listener when the adapter sets none.
pub const DEFAULT_MAX_BODY: usize = 16 * 1024 * 1024;

/// Reads the credential `name` from the owning profile's secrets store
/// only (never another profile's, never the environment).
///
/// # Errors
///
/// `missing_secret` (fatal) naming the secret and the profile;
/// `secrets_unreadable` (retryable) when the store cannot be read.
pub fn credential(scope: &ProfileScope, name: &str) -> Result<String, AdapterError> {
    let profile = scope.name().as_str();
    match scope.secrets().store().get(name) {
        Ok(Some(value)) => Ok(value.expose().to_owned()),
        Ok(None) => Err(AdapterError {
            retryable: false,
            code: "missing_secret".to_owned(),
            message: format!(
                "secret {name} is missing from profile '{profile}'; set it with `prometheus -p {profile} secrets set {name}`"
            ),
        }),
        Err(err) => Err(AdapterError {
            retryable: true,
            code: "secrets_unreadable".to_owned(),
            message: format!(
                "the secrets of profile '{profile}' cannot be read: {}",
                err.message()
            ),
        }),
    }
}

/// A tool call shown as a native task card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCard {
    /// Stable per card (the tool call's id).
    pub id: String,
    /// What it does (`terminal: ls -la`).
    pub title: String,
    /// Where it is.
    pub status: TaskStatus,
}

/// A task card's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Running.
    Running,
    /// Done.
    Done,
    /// Failed.
    Failed,
}

/// One message of a chat's history (`fetch_history`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The platform's message id.
    pub message_id: String,
    /// Its author's id.
    pub author_id: String,
    /// Its author's display name.
    pub author_name: String,
    /// The text.
    pub text: String,
    /// The bot itself wrote it.
    pub from_self: bool,
}

/// A chat's name and kind (`chat-info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    /// Display name.
    pub name: String,
    /// `dm`, `group`, `channel` or `thread`.
    pub chat_type: ChatType,
}

/// A download of an inbound attachment.
pub struct MediaStream {
    /// The length the platform declared, when it did.
    pub declared_len: Option<u64>,
    /// The body, chunk by chunk.
    pub chunks: Pin<Box<dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Send>>,
}

impl std::fmt::Debug for MediaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaStream")
            .field("declared_len", &self.declared_len)
            .finish_non_exhaustive()
    }
}

/// One step of streamed reply audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOp {
    /// A reply's audio starts in a chat.
    Begin {
        /// Chat id.
        chat_id: String,
        /// The turn it belongs to.
        stream_id: String,
    },
    /// More audio.
    Write {
        /// The stream.
        stream_id: String,
        /// Encoded audio.
        bytes: Vec<u8>,
    },
    /// The audio is complete.
    Finish {
        /// The stream.
        stream_id: String,
    },
    /// The audio is abandoned.
    Abort {
        /// The stream.
        stream_id: String,
    },
}

/// A request on the shared HTTP listener, below `/p/<profile>/<platform>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path below the adapter's prefix (`/` at least).
    pub path: String,
    /// The query string, without `?`.
    pub query: Option<String>,
    /// Headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The first header named `name` (lower case).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An adapter's answer on the shared listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// Other headers.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// An empty response with `status`.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A JSON response.
    #[must_use]
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            content_type: Some("application/json".to_owned()),
            body: body.to_string().into_bytes(),
            ..Self::status(status)
        }
    }

    /// The response with one more header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// A reaction to add or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// Chat id.
    pub chat_id: String,
    /// The message reacted to.
    pub message_id: String,
    /// The emoji.
    pub emoji: String,
    /// Remove instead of add.
    pub remove: bool,
}

/// What a factory builds an adapter from.
#[derive(Debug, Clone)]
pub struct AdapterSetup {
    /// The owning profile's scope (credentials come only from its secrets).
    pub scope: ProfileScope,
    /// Platform name.
    pub platform: String,
    /// The `[gateway.platforms.<name>]` table.
    pub config: PlatformConfig,
}

/// Builds the adapter of one platform for one profile.
pub type AdapterFactory =
    Arc<dyn Fn(&AdapterSetup) -> Result<Arc<dyn Adapter>, Error> + Send + Sync>;
