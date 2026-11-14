//! The seam between the turn loop and the desktop window (spec:
//! desktop-app, Desktop UI toolset contract; First-run onboarding chat;
//! projects, Project tool for GUI sessions): the daemon implements
//! [`DesktopPort`] over the session's attached desktop clients, its store
//! and its working directory.

use pmt_core::cancel::CancelToken;
use pmt_core::id::SessionId;
use pmt_tools::ToolResult;
use pmt_tools::agent_level_desktop::DESKTOP_UI_TOOLS;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// One session's desktop tools, as the daemon runs them.
pub trait DesktopPort: Send + Sync {
    /// Runs one call of tool `name`.
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolResult>;
}

/// The desktop tools of every session a factory opens.
pub trait DesktopPorts: Send + Sync {
    /// The port of `session`.
    fn port(&self, session: SessionId) -> std::sync::Arc<dyn DesktopPort>;
}

/// Whether the turn loop runs `name` through a [`DesktopPort`].
#[must_use]
pub fn handles(name: &str) -> bool {
    DESKTOP_UI_TOOLS.contains(&name) || matches!(name, "setup_beat" | "project")
}

/// What a desktop tool answers in a session without a port (no daemon).
#[must_use]
pub fn unavailable(name: &str) -> ToolResult {
    if name == "project" {
        return ToolResult::error("project is not available in this session");
    }
    no_window(name)
}

/// The structured error of a window tool with no desktop window attached
/// (spec: desktop-app, Desktop UI toolset contract).
#[must_use]
pub fn no_window(name: &str) -> ToolResult {
    ToolResult::text(
        pmt_tools::result::Status::Error,
        serde_json::json!({
            "success": false,
            "error": format!("{name} needs an open Prometheus Desktop window"),
        })
        .to_string(),
    )
}
