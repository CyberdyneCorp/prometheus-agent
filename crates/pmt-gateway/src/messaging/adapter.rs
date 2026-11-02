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
    /// Send without markup (the resend after a `formatting` refusal).
    pub plain: bool,
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
            plain: false,
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
}

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
            body: Vec::new(),
        }
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
