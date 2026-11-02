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

/// How an adapter renders choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interactive {
    /// Platform buttons.
    Native,
    /// A numbered list answered by number.
    Numbered,
    /// Slash-command instructions.
    None,
}

/// An adapter's own access policy, when it enforces one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPolicy {
    /// It submits only senders on its allowlist (trusted).
    Allowlist,
    /// It submits everyone (not trusted).
    Open,
}

/// What an adapter declares (the subset of the capability descriptor the
/// gateway core reads; the full descriptor is the platform adapters').
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent capability flags
pub struct Descriptor {
    /// Platform name.
    pub platform: String,
    /// Longest message, in the platform's unit.
    pub max_message_length: usize,
    /// Messages can be edited.
    pub supports_edit: bool,
    /// Messages can be deleted.
    pub supports_delete: bool,
    /// How choices are rendered.
    pub interactive: Interactive,
    /// The connector authorizes senders upstream (the relay only).
    pub authorization_is_upstream: bool,
    /// The adapter's own access policy.
    pub access_policy: Option<AccessPolicy>,
    /// It can send one message without a running connection (one-shot
    /// `gateway send` without the daemon).
    pub stateless_send: bool,
    /// It can add and remove reactions.
    pub supports_reactions: bool,
}

impl Descriptor {
    /// A plain descriptor for `platform`: no editing, numbered choices.
    #[must_use]
    pub fn new(platform: &str, max_message_length: usize) -> Self {
        Self {
            platform: platform.to_owned(),
            max_message_length,
            supports_edit: false,
            supports_delete: false,
            interactive: Interactive::Numbered,
            authorization_is_upstream: false,
            access_policy: None,
            stateless_send: false,
            supports_reactions: false,
        }
    }
}

/// Media kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// A picture.
    Image,
    /// An audio file.
    Audio,
    /// A voice note.
    Voice,
    /// A video.
    Video,
    /// Any other file.
    Document,
}

/// One inbound attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// What it is.
    pub kind: MediaKind,
    /// Where the gateway can read it.
    pub location: String,
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
    /// The profile a multiplexing connector (the relay) addresses; `None`
    /// is the adapter's own profile.
    pub profile: Option<String>,
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
            profile: None,
        }
    }
}

/// A button on an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Button {
    /// What it shows.
    pub label: String,
    /// What a press submits as [`Inbound::callback`].
    pub data: String,
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
    /// Files sent with it (from `MEDIA:` directives).
    pub attachments: Vec<OutboundAttachment>,
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
            attachments: Vec::new(),
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
}

/// The gateway, as an adapter reaches it.
pub trait AdapterLink: Send + Sync {
    /// Submits an inbound event.
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission>;
    /// Reports a state change.
    fn report(&self, state: AdapterState, error: Option<AdapterError>);
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
