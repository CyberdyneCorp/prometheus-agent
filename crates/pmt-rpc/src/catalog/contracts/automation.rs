//! `automation.state` (spec: session-automation, Automation status on
//! every surface): a session's goal, loop or heartbeat changed or a wakeup
//! was scheduled. Clients render a live row per kind (the CLI and TUI a
//! line such as `loop · every 5m · 2 runs · next in 4m`, the desktop a
//! card counting down to `next_fire_at`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Event;

/// One kind of a session's automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutomationStatePayload {
    /// The session.
    pub session_id: String,
    /// `goal`, `loop` or `heartbeat`.
    pub kind: String,
    /// `active`, `paused`, `done`, `parked`, or `none` once removed.
    pub status: String,
    /// The live row as the CLI shows it (absent once removed).
    #[serde(default)]
    pub row: Option<String>,
    /// Turns (goal), wakeups (loop) or fires (heartbeat) so far.
    #[serde(default)]
    pub count: u32,
    /// A goal's turn budget (`goals.max_turns`).
    #[serde(default)]
    pub limit: Option<u32>,
    /// When it fires next (RFC 3339), for an active loop or heartbeat or a
    /// parked goal's deadline.
    #[serde(default)]
    pub next_fire_at: Option<String>,
}

event!(/// `automation.state`: a session's goal, loop or heartbeat changed or
    /// a wakeup was scheduled.
    AutomationStateChanged = "automation.state" (AutomationStatePayload));
