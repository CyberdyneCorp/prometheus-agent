//! The registry entry of the `bot` toolset (spec: bot-mode, `message_agent`
//! tool; desktop-app, Agent-to-agent messages from bot chats): one tool,
//! `message_agent`, which only a Bot Mode bot's canonical Bot Chat gets
//! (the session adds it; no surface selects the toolset), so any other
//! session's dispatcher refuses the name as an unknown tool.

use serde_json::json;

use crate::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};

/// The toolset.
pub const TOOLSET: &str = "bot";

/// The tool.
pub const MESSAGE_AGENT: &str = "message_agent";

/// Every tool of the toolset.
pub const TOOLS: &[&str] = &[MESSAGE_AGENT];

/// The longest message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 16_000;

/// The `bot` toolset's tools.
#[must_use]
pub fn tools() -> Vec<RegisteredTool> {
    let mut d = ToolDescriptor::new(
        MESSAGE_AGENT,
        TOOLSET,
        "Send a message to another bot on your roster (a teammate). Returns at once with \
         status \"queued\" and a delivery_id; the teammate's reply (or why it failed, as \
         [reason: <code>]) arrives later as a message in this chat. target: a profile name, \
         an @handle from the roster, @prometheus for the primary bot, <peer>/<agent> or \
         <peer> for a peer gateway, or <handle>@<connection> for another desktop connection.",
        json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "Who to message (see the roster in your instructions)."},
                "message": {"type": "string", "description": "The message (at most 16000 characters)."}
            },
            "required": ["target", "message"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims::none());
    vec![RegisteredTool::agent_loop(d)]
}
