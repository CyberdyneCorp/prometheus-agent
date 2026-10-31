//! The seam between the turn loop and the memory subsystem (spec: memory,
//! Memory tool surface; Per-turn recall injection; Per-turn retry cap).
//!
//! `pmt-memory` implements [`MemoryPort`] for one session; the turn loop
//! calls [`MemoryPort::begin_turn`] before the first model call of every
//! turn (recall, per-turn counters) and routes `memory` tool calls to
//! [`MemoryPort::call`].

use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// Archival memory recalled for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recalled {
    /// The fenced block appended to this turn's user message on the wire
    /// (never stored, never in the system prompt).
    pub text: String,
    /// The recalled entry ids (`memory.recalled`).
    pub entry_ids: Vec<String>,
}

/// One session's memory, as the turn loop sees it.
pub trait MemoryPort: Send + Sync {
    /// A new user turn with `text`: resets per-turn state and returns this
    /// turn's recall, if any.
    fn begin_turn(&self, text: &str) -> Option<Recalled>;

    /// Runs one `memory` tool call.
    fn call(&self, arguments: Value) -> BoxFuture<'_, ToolResult>;
}
