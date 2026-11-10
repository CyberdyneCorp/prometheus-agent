//! `session.steer` (spec: agent-turn-loop, Structured steering).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Ack, Method};

/// How a steer is delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SteerKind {
    /// Inserted before the next model request.
    #[default]
    Note,
    /// Running cooperative tools yield; inserted as a real user message.
    Redirect,
    /// The current tool batch finishes, then one toolless request ends
    /// the turn as `completed`.
    WrapUp,
}

/// `session.steer` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    /// The session.
    pub session_id: String,
    /// The steer; empty or whitespace-only text is refused (`empty steer`).
    pub text: String,
    /// How it is delivered (default `note`).
    #[serde(default)]
    pub kind: SteerKind,
}

method!(/// `session.steer`: steers the session's running turn.
    SessionSteer = "session.steer" (SteerParams) -> Ack);
