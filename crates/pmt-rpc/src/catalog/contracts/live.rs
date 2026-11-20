//! The contracts of the TUI's M3 upgrades (spec: tui, Split panes for
//! subagents and tool output; Session timeline scrubber; Shared live
//! sessions): who is attached to a session (`session.presence`), a
//! delegated child's live transcript (`subagent.subscribe`, then
//! `subagent.event`), and a session's turns with the branch a rewind
//! makes (`session.timeline`, `session.rewind`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AttachRole, Event, Method, SessionRef};

/// One client attached to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresenceClient {
    /// The client's id within the daemon (`<kind>-<connection>`), the one
    /// its inputs are stamped with.
    pub client_id: String,
    /// Its kind from `rpc.hello` (`tui`, `desktop`, ...).
    pub client_kind: String,
    /// Driver or observer.
    pub role: AttachRole,
    /// When it attached (RFC 3339).
    pub since: String,
}

/// `session.presence` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionPresencePayload {
    /// The session.
    pub session_id: String,
    /// Every client attached now, in the order they attached.
    pub clients: Vec<PresenceClient>,
}

/// `subagent.subscribe` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentSubscribeParams {
    /// The parent session the connection is attached to.
    pub session_id: String,
    /// The child (`sa-<ULID>`).
    pub subagent_id: String,
}

/// `subagent.subscribe` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentSubscribed {
    /// The child.
    pub subagent_id: String,
    /// `running`, or how it ended (`completed`, `failed`, ...).
    pub status: String,
}

/// What one `subagent.event` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubagentEventKind {
    /// The child's assistant text.
    Assistant,
    /// A tool call it made (`tool`, `text` = what it works on).
    ToolCall,
    /// That call's result (`tool`, `text` = its output or error).
    ToolResult,
    /// It ended (`status`).
    Finished,
}

/// `subagent.event` payload: one item of a subscribed child's transcript,
/// sent to the subscribing connection only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentEventPayload {
    /// The parent session.
    pub session_id: String,
    /// The child.
    pub subagent_id: String,
    /// What it is.
    pub kind: SubagentEventKind,
    /// Its text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The tool, for a call or result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// How the child ended, for `finished`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One turn of a session's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineTurn {
    /// Its number, from 1.
    pub turn: u32,
    /// The user message that started it (`msg_<ULID>`).
    pub message_id: String,
    /// When it started (RFC 3339).
    pub at: String,
    /// The first line of its input.
    pub first_line: String,
    /// Tool calls it made.
    pub tools: u32,
    /// What its model calls cost, when known.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// `session.timeline` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Timeline {
    /// The active transcript's turns, oldest first.
    pub turns: Vec<TimelineTurn>,
}

/// `session.rewind` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RewindParams {
    /// The session.
    pub session_id: String,
    /// The last turn the branch keeps (from 1).
    pub to_turn: u32,
}

/// `session.rewind` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRewound {
    /// The new branch session, ending at `to_turn`.
    pub session_id: String,
    /// The session it branched from (kept unchanged).
    pub parent_session_id: String,
    /// The turns the branch holds.
    pub turns: u32,
    /// The workspace checkpoint holding the files as they were after
    /// `to_turn` (`files.rollback {checkpoint}`), when later turns changed
    /// files; restoring is the client's offer to make.
    #[serde(default)]
    pub checkpoint: Option<u32>,
}

event!(/// `session.presence`: a client attached to or left the session.
    SessionPresence = "session.presence" (SessionPresencePayload));
method!(/// `subagent.subscribe`: streams a delegated child's transcript to
    /// this connection as `subagent.event`s (what it did so far first).
    SubagentSubscribe = "subagent.subscribe" (SubagentSubscribeParams) -> SubagentSubscribed);
event!(/// `subagent.event`: one item of a subscribed child's transcript.
    SubagentStreamed = "subagent.event" (SubagentEventPayload));
method!(/// `session.timeline`: the session's turns (read only).
    SessionTimeline = "session.timeline" (SessionRef) -> Timeline);
method!(/// `session.rewind`: a new branch session ending at a turn; the
    /// original is kept.
    SessionRewind = "session.rewind" (RewindParams) -> SessionRewound);
