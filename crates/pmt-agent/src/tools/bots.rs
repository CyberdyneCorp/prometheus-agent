//! The seam between the turn loop and other bots (spec: bot-mode,
//! `message_agent` tool): the binary implements [`BotPort`] over the roster
//! and the durable delivery records of the session's profile.

use pmt_core::id::SessionId;
use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// Other bots, as `message_agent` reaches them.
pub trait BotPort: Send + Sync {
    /// Runs `message_agent` for `session`.
    fn message_agent(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult>;
}

/// What `message_agent` answers without a port.
pub const NO_BOTS: &str = "other bots are not reachable from this session";
