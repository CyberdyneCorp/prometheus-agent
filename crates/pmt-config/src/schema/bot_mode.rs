//! The `[bot_mode]` section (spec: bot-mode, agent-interop): whether the
//! profile is a bot, the bot protocol of its canonical Bot Chat, and how
//! bot messages are carried between bots (the desktop relay's bounds
//! included). The `[hosted_rooms]` section bounds group chat discussions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[bot_mode]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct BotModeConfig {
    /// The profile appears as a bot (rosters, bot messages, the bot
    /// protocol); `false` leaves its profile, sessions, routines and room
    /// history untouched but refuses incoming bot messages with
    /// `missing_config`.
    pub enabled: bool,
    /// Canonical Bot Chats get the bot-messaging protocol section and the
    /// `message_agent` tool.
    pub protocol: bool,
    /// Seconds a bot message waits for a busy target before it fails with
    /// `target_busy`.
    #[schemars(range(min = 1, max = 86_400))]
    pub turn_wait_secs: u64,
    /// Consecutive bot-originated hops after which a chain of bot messages
    /// is cut with `hop_limit`.
    #[schemars(range(min = 1, max = 64))]
    pub max_hops: u32,
    /// Seconds a relayed bot message's turn may take on the target before
    /// it fails.
    #[schemars(range(min = 1, max = 86_400))]
    pub relay_turn_timeout_secs: u64,
    /// Bot messages still queued after this many seconds fail with
    /// `queued_expired` when drained (`0`: never).
    pub envelope_ttl_secs: u64,
}

impl Default for BotModeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol: true,
            turn_wait_secs: 120,
            max_hops: 4,
            relay_turn_timeout_secs: 600,
            envelope_ttl_secs: 900,
        }
    }
}

/// The `[hosted_rooms]` section: group chats (rooms) this home drives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct HostedRoomsConfig {
    /// The bounds of one user message's discussion.
    pub discussion: DiscussionConfig,
}

/// `[hosted_rooms.discussion]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiscussionConfig {
    /// Serial rounds one user message drives at most.
    #[schemars(range(min = 1, max = 20))]
    pub max_rounds: u32,
    /// Member messages one user message produces at most.
    #[schemars(range(min = 1, max = 100))]
    pub max_messages: u32,
}

impl Default for DiscussionConfig {
    fn default() -> Self {
        Self {
            max_rounds: 3,
            max_messages: 10,
        }
    }
}
