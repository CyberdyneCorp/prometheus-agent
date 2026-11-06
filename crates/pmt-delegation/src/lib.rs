//! Subagent delegation (spec: subagent-delegation): the host-owned
//! lifecycle of child agents behind one contract shared by the
//! `delegate_task` tool, slash commands, RPC and the plugin lifecycle
//! interface.
//!
//! The crate is surface-neutral. [`contract`] and [`validate`] turn the
//! model's arguments into checked tasks; [`grant`] derives what a child may
//! do (only ever narrower than its parent); [`agents`] loads named
//! definitions; [`routing`] picks a child's model; [`budget`] splits call
//! budgets and counts one-shot spawns; [`limiter`] admits children under
//! the profile's concurrency limit with fair queueing; [`stall`] detects
//! stuck children; [`progress`] paces live progress events; [`result`]
//! builds the structured `SubagentResult`; [`tree`] scopes control
//! actions to the caller's spawn tree; [`router`] selects the delegation
//! router; [`handles`] signs plugin lifecycle handles; [`host`] runs it
//! all over a [`host::Launcher`] the binary implements with real sessions.

pub mod agents;
pub mod budget;
pub mod contract;
pub mod grant;
pub mod handles;
pub mod host;
pub mod ids;
pub mod limiter;
pub mod output;
pub mod progress;
pub mod result;
pub mod router;
pub mod routing;
pub mod schema_check;
pub mod stall;
pub mod summary;
pub mod tree;
pub mod validate;

use pmt_core::error::{Error, ErrorCode};
use pmt_tools::ToolResult;

/// A refusal of a call or a task: a stable code and the message the model
/// sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The error code (`delegation.depth_limit`...).
    pub code: &'static ErrorCode,
    /// The message.
    pub message: String,
}

impl Refusal {
    /// A refusal with `code` and `message`.
    #[must_use]
    pub fn new(code: &'static ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The tool error the model sees: the message as text, the code in
    /// the result's metadata.
    #[must_use]
    pub fn tool_result(&self) -> ToolResult {
        let mut result = ToolResult::error(self.message.clone());
        result
            .metadata
            .insert("code".to_owned(), self.code.code.into());
        result
    }

    /// The refusal as an [`Error`].
    #[must_use]
    pub fn error(&self) -> Error {
        Error::new(self.code, self.message.clone())
    }
}

impl From<&Error> for Refusal {
    fn from(err: &Error) -> Self {
        Self {
            code: err.code(),
            message: err.message().to_owned(),
        }
    }
}
