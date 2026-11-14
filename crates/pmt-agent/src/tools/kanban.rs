//! The seam between the turn loop and kanban boards (spec: kanban,
//! Kanban toolset and grants): the binary implements [`KanbanPort`] over
//! the home's boards, with the session's grant (a worker's own board and
//! run, or none).

use pmt_core::id::SessionId;
use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// Kanban boards, as the `kanban_*` tools reach them.
pub trait KanbanPort: Send + Sync {
    /// Runs the kanban tool `name` for `session`.
    fn call(&self, session: SessionId, name: &str, arguments: Value) -> BoxFuture<'_, ToolResult>;
}

/// What a kanban tool answers without a port.
pub const NO_KANBAN: &str = "kanban is not available in this session";

/// Whether `name` is a kanban tool.
#[must_use]
pub fn handles(name: &str) -> bool {
    name.starts_with("kanban_")
}
