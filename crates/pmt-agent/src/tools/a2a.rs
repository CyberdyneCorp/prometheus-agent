//! The seam between the turn loop and A2A agents (spec: agent-interop,
//! Outbound A2A tools): the binary implements [`A2aPort`] over the
//! profile's configured agents, its egress policy and grants, and its
//! store (conversations and metrics).

use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// A2A agents, as the `a2a_*` tools reach them.
pub trait A2aPort: Send + Sync {
    /// Runs the A2A tool `name`.
    fn call(&self, name: &str, arguments: Value) -> BoxFuture<'_, ToolResult>;
}

/// What an A2A tool answers without a port.
pub const NO_A2A: &str = "A2A agents are not available in this session";

/// Whether `name` is an A2A tool.
#[must_use]
pub fn handles(name: &str) -> bool {
    name.starts_with("a2a_")
}
