//! The loop's seams to subagent delegation and programmatic tool calling
//! (spec: subagent-delegation): `delegate_task` and `execute_code` need
//! the session's children, grant and tool dispatch, which live outside
//! this crate; the session wires its implementations in.

use pmt_core::cancel::CancelToken;
use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// The session's `delegate_task`.
pub trait DelegationPort: Send + Sync {
    /// Runs one call made in a turn whose hard stop is `cancel`.
    fn call<'a>(&'a self, arguments: Value, cancel: &'a CancelToken) -> BoxFuture<'a, ToolResult>;
}

/// The session's `execute_code`.
pub trait CodePort: Send + Sync {
    /// Runs one script made in a turn whose hard stop is `cancel`.
    fn call<'a>(&'a self, arguments: Value, cancel: &'a CancelToken) -> BoxFuture<'a, ToolResult>;
}

/// The answer when the session has no delegation.
pub const NO_DELEGATION: &str = "delegate_task is not available in this session.";
/// The answer when the session has no code execution.
pub const NO_CODE: &str = "execute_code is not available in this session.";
