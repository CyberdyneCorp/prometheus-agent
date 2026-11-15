//! What makes a session a kanban worker (spec: kanban, Kanban toolset and
//! grants; Workspaces; Orchestrator guidance): the `kanban-<task>`
//! source tag, no interactive tools, the `kanban` toolset without
//! `kanban_list` and `kanban_unblock`, a port scoped to its own board and
//! run, reads of the home's attachment blobs, and the worker guidance in
//! its frozen system prompt (with the orchestrator guidance for a profile
//! whose `kanban.role` is `orchestrator`).

use std::path::PathBuf;

use pmt_core::capability::{Capability, CapabilitySet};
use pmt_tools::agent_level_kanban::{NOT_WORKER, TOOLS};
use pmt_tools::{Selection, ToolRegistry};

use crate::kanban_host::tool::Grant;

/// A worker session's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanRun {
    /// Its board, task and run.
    pub grant: Grant,
    /// `<home>/kanban/blobs`: attachments the session's file tools read.
    pub blobs: PathBuf,
    /// The profile's `kanban.role` is `orchestrator`.
    pub orchestrator: bool,
}

impl KanbanRun {
    /// The session's source tag: `kanban-<task>`.
    #[must_use]
    pub fn source_tag(&self) -> String {
        format!("{}-{}", super::KANBAN_SOURCE_TAG, self.grant.task)
    }

    /// The worker's tools: `base` without interactive tools, plus the
    /// worker's share of the `kanban` toolset.
    #[must_use]
    pub fn selection(&self, registry: &ToolRegistry, base: Selection) -> Selection {
        let mut selection = base.without_interactive(registry);
        for tool in TOOLS.iter().filter(|t| !NOT_WORKER.contains(t)) {
            if registry.get(tool).is_ok() && !selection.tools.iter().any(|t| t == tool) {
                selection.tools.push((*tool).to_owned());
                selection.disabled.remove(*tool);
            }
        }
        selection
    }

    /// Reads of the attachment blobs.
    #[must_use]
    pub fn sandbox_grants(&self) -> CapabilitySet {
        let dir = std::fs::canonicalize(&self.blobs).unwrap_or_else(|_| self.blobs.clone());
        std::iter::once(Capability::read_under(&dir)).collect()
    }

    /// The worker guidance of the frozen system prompt, and an
    /// orchestrator's guidance after it.
    #[must_use]
    pub fn guidance(&self) -> String {
        let mut text = pmt_kanban::context::WORKER_GUIDANCE.to_owned();
        if self.orchestrator {
            text.push_str("\n\n");
            text.push_str(crate::kanban_host::orchestrate::ORCHESTRATOR_GUIDANCE);
        }
        text
    }
}
