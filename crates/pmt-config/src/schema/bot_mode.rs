//! The `[bot_mode]` section (spec: bot-mode, agent-interop): how bot
//! messages are carried between bots. The desktop relay's bounds live
//! here; Bot Mode's own keys join them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[bot_mode]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct BotModeConfig {
    /// Seconds a relayed bot message's turn may take on the target before
    /// it fails.
    #[schemars(range(min = 1, max = 86_400))]
    pub relay_turn_timeout_secs: u64,
    /// Envelopes still queued (never drained by a courier) after this many
    /// seconds fail with `queued_expired` (`0`: never).
    pub envelope_ttl_secs: u64,
}

impl Default for BotModeConfig {
    fn default() -> Self {
        Self {
            relay_turn_timeout_secs: 600,
            envelope_ttl_secs: 900,
        }
    }
}
