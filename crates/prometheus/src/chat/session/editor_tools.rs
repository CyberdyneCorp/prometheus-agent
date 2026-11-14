//! The turn-start hook of editor sessions (spec: acp-adapter, Editor MCP
//! servers belong to the daemon session): a changed editor server list
//! applies from the next turn.

use super::Live;

impl Live {
    /// Rebuilds the session's tools when an editor bound other MCP servers
    /// since they were built; a failure keeps the tools it has.
    pub async fn refresh_editor_tools(&self) {
        match self.tool_state.refresh_editor().await {
            Ok(true) => tracing::info!(session = %self.id, "editor MCP tools applied"),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                session = %self.id,
                "editor MCP tools not applied; keeping the session's tools: {err}"
            ),
        }
    }
}
