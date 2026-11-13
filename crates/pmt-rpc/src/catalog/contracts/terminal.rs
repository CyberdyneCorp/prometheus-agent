//! Daemon-owned terminals and the live subagents frame (spec: desktop-app,
//! Embedded terminals are daemon-owned PTYs; Live subagents frame).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method};

/// `pty.open` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyOpenParams {
    /// The session whose workspace the shell starts in (attached as a
    /// driver).
    pub session_id: String,
    /// Columns.
    #[serde(default = "cols")]
    pub cols: u16,
    /// Rows.
    #[serde(default = "rows")]
    pub rows: u16,
}

const fn cols() -> u16 {
    80
}
const fn rows() -> u16 {
    24
}

/// A terminal of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyRef {
    /// The session.
    pub session_id: String,
    /// The terminal.
    pub pty_id: String,
}

/// `pty.open` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyOpened {
    /// The new terminal.
    pub pty_id: String,
}

/// `pty.attach` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyAttached {
    /// The terminal.
    pub pty_id: String,
    /// Its recent output (at most 1 MiB), base64.
    pub replay: String,
}

/// `pty.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyList {
    /// The session's terminals.
    pub terminals: Vec<String>,
}

/// `pty.input` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyInputParams {
    /// The session.
    pub session_id: String,
    /// The terminal.
    pub pty_id: String,
    /// What is typed (UTF-8 text, control characters included).
    pub data: String,
}

/// `pty.resize` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyResizeParams {
    /// The session.
    pub session_id: String,
    /// The terminal.
    pub pty_id: String,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// `pty.output` and `pty.exited` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyOutput {
    /// The terminal.
    pub pty_id: String,
    /// The output, base64 (empty for `pty.exited`).
    #[serde(default)]
    pub data: String,
}

method!(/// `pty.open`: a shell the daemon spawns as the user in the session's
    /// workspace (drivers only; at most 16 per daemon).
    PtyOpen = "pty.open" (PtyOpenParams) -> PtyOpened);
method!(/// `pty.attach`: a terminal's recent output, to show before its live
    /// `pty.output`.
    PtyAttach = "pty.attach" (PtyRef) -> PtyAttached);
method!(/// `pty.list`: the session's terminals.
    PtyListCall = "pty.list" (super::SessionRef) -> PtyList);
method!(/// `pty.input`: types into a terminal (drivers only).
    PtyInput = "pty.input" (PtyInputParams) -> super::Ack);
method!(/// `pty.resize`: a terminal's size (drivers only).
    PtyResize = "pty.resize" (PtyResizeParams) -> super::Ack);
method!(/// `pty.close`: ends the shell (drivers only).
    PtyClose = "pty.close" (PtyRef) -> super::Ack);
event!(/// `pty.output`: a terminal printed.
    PtyOutputEvent = "pty.output" (PtyOutput));
event!(/// `pty.exited`: a terminal's shell ended.
    PtyExited = "pty.exited" (PtyOutput));

/// `subagent.steer` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentSteerParams {
    /// The parent session.
    pub session_id: String,
    /// The child.
    pub subagent_id: String,
    /// The guidance.
    pub text: String,
}

/// `subagent.stop` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentRef {
    /// The parent session.
    pub session_id: String,
    /// The child.
    pub subagent_id: String,
}

/// What a control call did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentControlled {
    /// The child.
    pub subagent_id: String,
    /// `queued` (steer: delivered at the child's next checkpoint) or
    /// `interrupt_requested` (stop).
    pub status: String,
}

method!(/// `subagent.steer`: queues guidance for a running child's next
    /// checkpoint (drivers only).
    SubagentSteer = "subagent.steer" (SubagentSteerParams) -> SubagentControlled);
method!(/// `subagent.stop`: stops a running child (drivers only).
    SubagentStop = "subagent.stop" (SubagentRef) -> SubagentControlled);
