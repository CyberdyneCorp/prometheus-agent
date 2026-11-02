//! The `[gateway]` section: how a profile takes part in the home's gateway
//! daemon (spec: configuration-and-profiles, Served profile set) and how
//! its messaging conversations are routed, queued, authorized and
//! delivered (spec: messaging-gateway). Daemon-wide gateway keys
//! (`gateway.http.*`, `gateway.relay.*`, `gateway.on_all_adapters_down`)
//! live in `daemon.toml`, never here.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use super::gateway_policy::{
    ChatPolicy, HomeChannel, PolicyChatType, PolicyFacts, RateLimitConfig, RateLimitOverride,
    resolve_policy,
};
use super::{BusyInputMode, SecretName};

/// The `[gateway]` section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct GatewayConfig {
    /// In the daemon's launch profile: serve every profile of the home
    /// (each in its own scope) instead of the launch profile only.
    pub multiplex: bool,
    /// Never serve this profile from a multiplexing daemon (it gets a
    /// daemon of its own home).
    pub standalone: bool,
    /// Messaging platforms this profile answers on, keyed by platform
    /// (`telegram`, `discord`, ...). A profile clone leaves them out
    /// unless `--clone-channels` is given, so two profiles never answer
    /// the same bot account.
    #[schemars(extend("x-apply" = "next_session"))]
    pub platforms: BTreeMap<String, PlatformConfig>,
    /// How conversations map to sessions.
    pub sessions: GatewaySessions,
    /// Input that arrives while a turn runs (the mode itself is
    /// `busy.input_mode`, overridable per platform).
    #[schemars(extend("x-apply" = "live"))]
    pub busy: GatewayBusy,
    /// `exec` quick commands run from a chat.
    #[schemars(extend("x-apply" = "live"))]
    pub quick_commands: GatewayQuickCommands,
    /// Token streaming to platforms.
    #[schemars(extend("x-apply" = "live"))]
    pub streaming: StreamingConfig,
    /// Notifications of background work.
    #[schemars(extend("x-apply" = "live"))]
    pub notifications: NotificationsConfig,
    /// The durable delivery outbox.
    pub outbox: OutboxConfig,
    /// Stopping the daemon (read from the launch profile).
    pub shutdown: ShutdownConfig,
    /// Live agents kept in memory (read from the launch profile).
    pub agent_cache: AgentCacheConfig,
    /// Per-sender rate limits (overridable per platform and chat policy).
    #[schemars(extend("x-apply" = "live"))]
    pub rate_limit: RateLimitConfig,
    /// Per-chat policies; each applies to sessions created after it
    /// changed (`/policy apply --now` replaces a chat's session at once).
    #[schemars(extend("x-apply" = "next_session"))]
    pub chat_policies: Vec<ChatPolicy>,
}

/// `[gateway.sessions]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewaySessions {
    /// Each member of a non-thread group or channel gets a session of
    /// their own.
    pub group_per_user: bool,
    /// Each participant of a thread gets a session of their own.
    pub thread_per_user: bool,
    /// Routes (conversation to session, never transcripts) unused this
    /// long are pruned; `0` keeps them forever.
    pub route_max_age_secs: u64,
    /// A session whose turn was running during this many consecutive
    /// daemon restarts is suspended; its next message starts a new one.
    #[schemars(range(min = 1, max = 100))]
    pub stuck_loop_threshold: u32,
}

impl Default for GatewaySessions {
    fn default() -> Self {
        Self {
            group_per_user: true,
            thread_per_user: false,
            route_max_age_secs: 7_776_000,
            stuck_loop_threshold: 3,
        }
    }
}

/// `[gateway.busy]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayBusy {
    /// Text arriving within this many milliseconds of the previous text
    /// joins it into one input.
    #[schemars(range(max = 10_000))]
    pub debounce_ms: u64,
    /// An input is never held longer than this for more text.
    #[schemars(range(max = 60_000))]
    pub debounce_cap_ms: u64,
    /// Send one busy acknowledgement per conversation per 30 seconds.
    pub ack: bool,
    /// `/queue` entries kept per conversation.
    #[schemars(range(min = 1, max = 1000))]
    pub max_queued: u32,
}

impl Default for GatewayBusy {
    fn default() -> Self {
        Self {
            debounce_ms: 350,
            debounce_cap_ms: 1000,
            ack: true,
            max_queued: 16,
        }
    }
}

/// `[gateway.quick_commands]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayQuickCommands {
    /// An `exec` quick command run from a chat is killed, with its whole
    /// process tree, after this many seconds.
    #[schemars(range(min = 1, max = 3600))]
    pub timeout_secs: u64,
}

impl Default for GatewayQuickCommands {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

/// `[gateway.streaming]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Stream answers while they are written. A platform can turn it off
    /// for itself, never on when this is off.
    pub enabled: bool,
    /// How a streamed draft reaches the platform.
    pub transport: StreamTransport,
    /// Least time between two edits of a draft.
    #[schemars(range(min = 100, max = 60_000))]
    pub edit_interval_ms: u64,
    /// Characters gathered before the first draft is sent.
    #[schemars(range(min = 1, max = 4096))]
    pub buffer_threshold: u32,
    /// Shown at the end of a draft still being written.
    pub cursor: String,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: StreamTransport::Auto,
            edit_interval_ms: 800,
            buffer_threshold: 24,
            cursor: " \u{2589}".to_owned(),
        }
    }
}

/// How a streamed draft reaches a platform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamTransport {
    /// `edit` where the platform edits messages, else no streaming.
    #[default]
    Auto,
    /// A platform-native draft (falls back to `edit`).
    Draft,
    /// One message edited as the answer grows.
    Edit,
    /// No streaming.
    Off,
}

/// `[gateway.notifications]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Which background process and subagent events reach the chat.
    pub background: BackgroundNotifications,
}

/// Background notification levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundNotifications {
    /// Results and errors, one line each.
    #[default]
    Concise,
    /// Every event.
    All,
    /// Results only.
    Result,
    /// Errors only.
    Error,
    /// Nothing.
    Off,
}

/// `[gateway.outbox]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    /// Send attempts before an entry becomes `failed`.
    #[schemars(range(min = 1, max = 100))]
    pub max_attempts: u32,
    /// An entry older than this becomes `failed`.
    #[schemars(range(min = 1))]
    pub max_age_secs: u64,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            max_age_secs: 86_400,
        }
    }
}

/// `[gateway.shutdown]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShutdownConfig {
    /// A stop whose conversations finish within this many seconds is
    /// clean: the next start skips crash recovery.
    #[schemars(range(min = 1, max = 3600))]
    pub drain_timeout_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: 30,
        }
    }
}

/// `[gateway.agent_cache]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AgentCacheConfig {
    /// Conversations whose agent stays loaded.
    #[schemars(range(min = 1, max = 100_000))]
    pub max_size: u32,
    /// An agent unused this long is unloaded (its session stays open).
    #[schemars(range(min = 1))]
    pub idle_ttl_secs: u64,
    /// Above this resident memory the least recently used agents are
    /// unloaded; `0` is half the physical memory.
    pub memory_high_bytes: u64,
}

impl Default for AgentCacheConfig {
    fn default() -> Self {
        Self {
            max_size: 128,
            idle_ttl_secs: 3600,
            memory_high_bytes: 0,
        }
    }
}

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
    /// The secret holding the adapter's credential (never the value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<SecretName>,
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

/// The busy settings in effect for one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveBusy {
    /// The input mode.
    pub input_mode: BusyInputMode,
    /// Debounce window.
    pub debounce_ms: u64,
    /// Debounce cap.
    pub debounce_cap_ms: u64,
    /// Busy acknowledgements.
    pub ack: bool,
    /// `/queue` limit.
    pub max_queued: u32,
}

impl GatewayConfig {
    /// The table of `platform`, or the defaults when it has none.
    #[must_use]
    pub fn platform(&self, platform: &str) -> PlatformConfig {
        self.platforms.get(platform).cloned().unwrap_or_default()
    }

    /// The busy settings of `platform`: its overrides over `gateway.busy`
    /// and the shared `busy.input_mode`.
    #[must_use]
    pub fn busy_for(&self, platform: &str, shared: BusyInputMode) -> EffectiveBusy {
        let over = self.platform(platform).busy;
        EffectiveBusy {
            input_mode: over.input_mode.unwrap_or(shared),
            debounce_ms: over.debounce_ms.unwrap_or(self.busy.debounce_ms),
            debounce_cap_ms: over.debounce_cap_ms.unwrap_or(self.busy.debounce_cap_ms),
            ack: over.ack.unwrap_or(self.busy.ack),
            max_queued: over.max_queued.unwrap_or(self.busy.max_queued),
        }
    }

    /// `(group_per_user, thread_per_user)` of `platform`.
    #[must_use]
    pub fn sessions_for(&self, platform: &str) -> (bool, bool) {
        let over = self.platform(platform).sessions;
        (
            over.group_per_user.unwrap_or(self.sessions.group_per_user),
            over.thread_per_user
                .unwrap_or(self.sessions.thread_per_user),
        )
    }

    /// Whether answers stream on `platform`: only when streaming is on
    /// globally and the platform did not turn it off.
    #[must_use]
    pub fn streams_on(&self, platform: &str) -> bool {
        self.streaming.enabled
            && self.streaming.transport != StreamTransport::Off
            && self.platform(platform).streaming.enabled != Some(false)
    }

    /// The rate limits of `platform`: its overrides over
    /// `gateway.rate_limit`.
    #[must_use]
    pub fn rate_limit_for(&self, platform: &str) -> RateLimitConfig {
        self.rate_limit.with(&self.platform(platform).rate_limit)
    }

    /// What an unauthorized direct message on `platform` gets.
    #[must_use]
    pub fn unauthorized_dm(&self, platform: &str) -> UnauthorizedDm {
        let table = self.platform(platform);
        table
            .unauthorized_dm
            .unwrap_or(if platform == "email" || !table.allow_users.is_empty() {
                UnauthorizedDm::Ignore
            } else {
                UnauthorizedDm::Pair
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_override_wins_over_the_shared_busy_mode() {
        let mut gateway = GatewayConfig::default();
        gateway.platforms.insert(
            "telegram".into(),
            PlatformConfig {
                busy: PlatformBusy {
                    input_mode: Some(BusyInputMode::Steer),
                    ack: Some(false),
                    ..PlatformBusy::default()
                },
                ..PlatformConfig::default()
            },
        );
        let telegram = gateway.busy_for("telegram", BusyInputMode::Queue);
        assert_eq!(telegram.input_mode, BusyInputMode::Steer);
        assert!(!telegram.ack);
        assert_eq!(telegram.debounce_ms, 350);
        let discord = gateway.busy_for("discord", BusyInputMode::Interrupt);
        assert_eq!(discord.input_mode, BusyInputMode::Interrupt);
        assert!(discord.ack);
    }

    #[test]
    fn a_platform_cannot_enable_streaming_that_is_off_globally() {
        let mut gateway = GatewayConfig::default();
        gateway.platforms.insert(
            "telegram".into(),
            PlatformConfig {
                streaming: PlatformStreaming {
                    enabled: Some(true),
                },
                ..PlatformConfig::default()
            },
        );
        assert!(!gateway.streams_on("telegram"));
        gateway.streaming.enabled = true;
        assert!(gateway.streams_on("telegram"));
        gateway
            .platforms
            .get_mut("telegram")
            .unwrap()
            .streaming
            .enabled = Some(false);
        assert!(!gateway.streams_on("telegram"));
        assert!(gateway.streams_on("discord"));
    }

    #[test]
    fn unauthorized_dm_defaults_follow_the_platform_and_allowlist() {
        let mut gateway = GatewayConfig::default();
        assert_eq!(gateway.unauthorized_dm("telegram"), UnauthorizedDm::Pair);
        assert_eq!(gateway.unauthorized_dm("email"), UnauthorizedDm::Ignore);
        gateway.platforms.insert(
            "discord".into(),
            PlatformConfig {
                allow_users: vec!["1".into()],
                ..PlatformConfig::default()
            },
        );
        assert_eq!(gateway.unauthorized_dm("discord"), UnauthorizedDm::Ignore);
    }
}
