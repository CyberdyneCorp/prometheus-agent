//! `[gateway.platforms.<name>]` tables and the gateway-wide delivery keys
//! the platform adapters share (spec: platform-adapters, Platform
//! configuration shape; Inbound deduplication and text batching in the
//! host; Oversized output handling; Inbound media limits and caching;
//! Voice input transcription; Voice replies and streaming TTS; Typing
//! indicator and human delay). Platform names are lower case; which names
//! exist and what each adapter's `options` table accepts is
//! [`crate::platforms`].

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::gateway_policy::{HomeChannel, RateLimitOverride};
pub use super::webhook::WebhookRoute;
use super::{BusyInputMode, SecretName};

/// One `[gateway.platforms.<name>]` table: the keys every adapter shares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformConfig {
    /// Start this adapter with the daemon.
    pub enabled: bool,
    /// The chat that receives scheduled output, platform-only send
    /// targets and notifications without an originating chat (`/sethome`
    /// sets it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_channel: Option<HomeChannel>,
    /// Which parts of a reply quote the message they answer: none, the
    /// first part, or every part. Unset: `first`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_mode: Option<ReplyToMode>,
    /// Show the platform's typing indicator while a turn runs. Unset:
    /// `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_indicator: Option<bool>,
    /// Lowers the adapter's message length limit (in its own unit); it
    /// can never raise it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 16))]
    pub max_message_length: Option<u32>,
    /// Platform user ids admitted without pairing (no wildcards: use
    /// `allow_all`).
    pub allow_users: Vec<String>,
    /// Chats every member of which is admitted.
    pub allow_chats: Vec<String>,
    /// Admit everyone (logged at WARN on every start).
    pub allow_all: bool,
    /// User ids with every command in direct messages.
    pub admins: Vec<String>,
    /// User ids with every command in groups.
    pub group_admins: Vec<String>,
    /// Commands other users may run where an admin list is set, besides
    /// `/help`, `/whoami`, `/stop`, `/new`, `/queue` and `/link`.
    pub user_commands: Vec<String>,
    /// What an unauthorized direct message gets: a pairing code (`pair`),
    /// nothing (`ignore`) or a refusal (`decline`). Unset: `ignore` for
    /// email or when `allow_users` is set, else `pair`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unauthorized_dm: Option<UnauthorizedDm>,
    /// Busy handling on this platform (overrides `busy.input_mode` and
    /// `gateway.busy`).
    pub busy: PlatformBusy,
    /// Session mapping on this platform (overrides `gateway.sessions`).
    pub sessions: PlatformSessions,
    /// Streaming on this platform.
    pub streaming: PlatformStreaming,
    /// Rate limits on this platform (overrides `gateway.rate_limit`).
    pub rate_limit: RateLimitOverride,
    /// Joins rapid consecutive text of one sender into one input (a
    /// client that splits a long paste); replaces the busy debounce on
    /// this platform when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batching: Option<Batching>,
    /// A uniformly random delay `[min, max]` in milliseconds before each
    /// outbound part. Unset: none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_delay_ms: Option<[u64; 2]>,
    /// The secret holding the adapter's credential (never the value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<SecretName>,
    /// The adapter's own settings, validated against the schema it
    /// publishes (`prometheus gateway platforms` lists the adapters).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, serde_json::Value>,
    /// Webhook routes by name (`webhook` only), each served at
    /// `/p/<profile>/webhook/<route>` on the shared listener.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, WebhookRoute>,
    /// Largest request body a webhook route reads (`webhook` only), in
    /// bytes; larger requests are answered 413 unread. Unset: 1048576.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1024, max = 16_777_216))]
    pub max_body_bytes: Option<u64>,
    /// The proxy this adapter's outbound HTTP and WebSocket traffic uses
    /// (`http://`, `https://` or `socks5://`, without a password; a URL
    /// with credentials goes in a secret named by `proxy_url_secret`).
    /// Unset: `gateway.proxy_url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// The secret holding this adapter's proxy URL when it carries
    /// credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url_secret: Option<SecretName>,
    /// Mark the triggering message with processing reactions (eyes, then a
    /// check mark or a cross). Unset: on for Discord and Slack, off
    /// elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactions: Option<bool>,
    /// Post one notice to this platform's home channel after a graceful
    /// restart or update that interrupted running turns (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_notification: Option<bool>,
}

impl PlatformConfig {
    /// Whether restart notices go to this platform's home channel.
    #[must_use]
    pub fn restart_notification_on(&self) -> bool {
        self.restart_notification.unwrap_or(true)
    }

    /// Whether the typing indicator is shown (default `true`).
    #[must_use]
    pub fn typing(&self) -> bool {
        self.typing_indicator.unwrap_or(true)
    }

    /// Whether processing reactions are on for `platform` (spec:
    /// platform-adapters, Processing lifecycle reactions).
    #[must_use]
    pub fn reactions_on(&self, platform: &str) -> bool {
        self.reactions.unwrap_or(matches!(
            platform,
            "discord" | "slack" | "feishu" | "dingtalk"
        ))
    }

    /// The webhook body limit in effect.
    #[must_use]
    pub fn webhook_body_limit(&self) -> u64 {
        self.max_body_bytes.unwrap_or(1_048_576)
    }

    /// The reply-to mode (default `first`).
    #[must_use]
    pub fn reply_to(&self) -> ReplyToMode {
        self.reply_to_mode.unwrap_or_default()
    }
}

/// Which parts of a reply quote the message they answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplyToMode {
    /// None.
    Off,
    /// The first part.
    #[default]
    First,
    /// Every part.
    All,
}

/// `[gateway.platforms.<name>.batching]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Batching {
    /// Text from the same sender within this many milliseconds joins the
    /// pending input.
    #[schemars(range(max = 2000))]
    pub delay_ms: u64,
    /// The wait after a part that filled the platform's limit (the client
    /// probably split a longer message); at least `delay_ms`.
    #[schemars(range(max = 4000))]
    pub split_delay_ms: u64,
}

impl Default for Batching {
    fn default() -> Self {
        Self {
            delay_ms: 300,
            split_delay_ms: 1000,
        }
    }
}

/// What an unauthorized direct message gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnauthorizedDm {
    /// A pairing code the owner can approve.
    Pair,
    /// Nothing.
    Ignore,
    /// A short refusal.
    Decline,
}

/// `[gateway.platforms.<name>.busy]`: unset keys follow `busy.input_mode`
/// and `gateway.busy`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformBusy {
    /// Overrides `busy.input_mode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_mode: Option<BusyInputMode>,
    /// Overrides `gateway.busy.debounce_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 10_000))]
    pub debounce_ms: Option<u64>,
    /// Overrides `gateway.busy.debounce_cap_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 60_000))]
    pub debounce_cap_ms: Option<u64>,
    /// Overrides `gateway.busy.ack`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack: Option<bool>,
    /// Overrides `gateway.busy.max_queued`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_queued: Option<u32>,
}

/// `[gateway.platforms.<name>.sessions]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformSessions {
    /// Overrides `gateway.sessions.group_per_user`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_per_user: Option<bool>,
    /// Overrides `gateway.sessions.thread_per_user`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_per_user: Option<bool>,
}

/// `[gateway.platforms.<name>.streaming]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformStreaming {
    /// `false` turns streaming off for this platform; `true` cannot turn
    /// it on while `gateway.streaming.enabled` is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `[gateway.dedup]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DedupConfig {
    /// A redelivered inbound event within this many seconds is dropped
    /// as a duplicate.
    #[schemars(range(min = 1, max = 86_400))]
    pub ttl_secs: u64,
    /// Message ids remembered per adapter (the oldest are forgotten
    /// first).
    #[schemars(range(min = 1, max = 1_000_000))]
    pub max_entries: u32,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 300,
            max_entries: 2000,
        }
    }
}

/// `[gateway.delivery]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    /// A reply that would need more parts is sent as its first part plus
    /// a truncation notice, with the full text attached.
    #[schemars(range(min = 2, max = 100))]
    pub max_parts: u32,
    /// Deliver files a final reply names by bare path (outside code) as
    /// uploads, removing the path from the text.
    pub detect_paths: bool,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            max_parts: 10,
            detect_paths: true,
        }
    }
}

/// `[gateway.media]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// Largest inbound attachment downloaded, in bytes; `0` disables the
    /// cap.
    pub max_inbound_bytes: u64,
    /// Cached inbound media older than this many seconds is deleted
    /// unless a kept session references it.
    #[schemars(range(min = 60))]
    pub cache_max_age_secs: u64,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            max_inbound_bytes: 134_217_728,
            cache_max_age_secs: 86_400,
        }
    }
}

/// `[gateway.voice]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    /// Transcribe inbound voice and audio clips with the profile's
    /// speech-to-text provider.
    pub stt_enabled: bool,
    /// Echo a transcript back to the chat once.
    pub echo_transcripts: bool,
    /// When replies are spoken.
    pub reply_mode: VoiceReplyMode,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            stt_enabled: true,
            echo_transcripts: true,
            reply_mode: VoiceReplyMode::Mirror,
        }
    }
}

/// When replies are spoken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceReplyMode {
    /// Never.
    Off,
    /// When the user's message was voice.
    #[default]
    Mirror,
    /// Always.
    Always,
}

/// Deserializes the platform tables with lower-case names (`Telegram` is
/// `telegram`); two tables naming one platform are refused.
///
/// # Errors
///
/// A duplicate platform after normalization.
pub(super) fn lower_case_platforms<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, PlatformConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = BTreeMap::<String, PlatformConfig>::deserialize(deserializer)?;
    let mut platforms = BTreeMap::new();
    for (name, table) in raw {
        let lower = name.to_lowercase();
        if platforms.insert(lower.clone(), table).is_some() {
            return Err(serde::de::Error::custom(format!(
                "gateway.platforms: two tables name platform '{lower}'"
            )));
        }
    }
    Ok(platforms)
}
