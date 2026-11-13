//! The desktop chat's daemon-held state (spec: desktop-app, Composer
//! history and queue editing; Desktop status bar; Composer model picker):
//! the session's prompt queue, which every attached window shows and
//! which survives app and daemon restarts, and the context breakdown of
//! the status bar's meter.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method, SessionRef};

/// One input waiting in a session's queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEntry {
    /// The input's turn id.
    pub id: String,
    /// The prompt text.
    pub text: String,
    /// A turn whose outcome was lost when the daemon stopped during it:
    /// it blocks the queue until it is deleted (Discard), and is never
    /// sent again.
    #[serde(default)]
    pub lost: bool,
}

/// A session's queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueState {
    /// The session.
    pub session_id: String,
    /// In run order; a lost turn comes first.
    pub entries: Vec<QueueEntry>,
    /// Paused: no queued input starts until `queue.resume`.
    pub paused: bool,
}

/// `queue.edit` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueEditParams {
    /// The session.
    pub session_id: String,
    /// The queued input.
    pub id: String,
    /// Its new text.
    pub text: String,
}

/// `queue.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueItemParams {
    /// The session.
    pub session_id: String,
    /// The queued input (or the lost turn, to discard it).
    pub id: String,
}

method!(/// `queue.list`: the session's queued inputs.
    QueueList = "queue.list" (SessionRef) -> QueueState);
method!(/// `queue.edit`: replaces a queued input's text.
    QueueEdit = "queue.edit" (QueueEditParams) -> QueueState);
method!(/// `queue.delete`: drops a queued input; deleting the lost turn
    /// discards it and unblocks the queue without re-sending it.
    QueueDelete = "queue.delete" (QueueItemParams) -> QueueState);
method!(/// `queue.pause`: holds the queue; the running turn is not touched.
    QueuePause = "queue.pause" (SessionRef) -> QueueState);
method!(/// `queue.resume`: lets queued inputs run again.
    QueueResume = "queue.resume" (SessionRef) -> QueueState);
event!(/// `queue.changed`: a session's queue changed (every attached client
    /// shows the same list).
    QueueChanged = "queue.changed" (QueueState));

/// One category of `session.context_usage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextCategory {
    /// `system_prompt`, `tool_definitions`, `skills`, `memory`, `rules`,
    /// `mcp`, `subagent_definitions` or `conversation`.
    pub category: String,
    /// Its estimated tokens.
    pub tokens: u64,
}

/// `model.options` params: a session's options, or a profile's before
/// any session exists (the composer's picker of a new chat).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionsParams {
    /// The session (it must be attached).
    #[serde(default)]
    pub session_id: Option<String>,
    /// The profile, when no session is named.
    #[serde(default)]
    pub profile: Option<String>,
}
