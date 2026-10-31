//! The agent-level tools (spec: tool-system, Agent-level tools are owned by
//! the turn loop): their registry entries are `pmt_tools::agent_level`
//! (generic dispatch refuses them); the turn loop runs them here, where the
//! session state they need lives.

pub mod clarify;
pub mod memory;
pub mod session_search;
pub mod todo;

use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::ToolResult;
use serde_json::Value;

pub use clarify::Clarifier;
pub use memory::{MemoryPort, Recalled};
pub use session_search::SessionSearch;
pub use todo::TodoList;

/// One session's agent-level tools.
pub struct AgentTools {
    store: Arc<Store>,
    session: SessionId,
    todo: Mutex<TodoList>,
    clarify: Clarifier,
    search: SessionSearch,
    memory: Option<Arc<dyn MemoryPort>>,
}

impl AgentTools {
    /// The tools of `session`: its stored todo list, `clarify` over
    /// `clarifier` and `session_search` over the session's profile `store`.
    ///
    /// # Errors
    ///
    /// The stored todo list could not be read.
    pub fn new(
        store: Arc<Store>,
        session: SessionId,
        clarifier: Clarifier,
        search: SessionSearch,
    ) -> Result<Self, pmt_core::Error> {
        let todo = TodoList::load(&store, session)?;
        Ok(Self {
            store,
            session,
            todo: Mutex::new(todo),
            clarify: clarifier,
            search,
            memory: None,
        })
    }

    /// Runs `memory` calls through `memory` (pass the same port as
    /// `AgentSetup::memory`).
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryPort>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Runs `name` when it is an agent-level tool this loop executes;
    /// `None` for any other tool (the generic dispatcher runs it).
    pub async fn call(&self, name: &str, arguments: Value) -> Option<ToolResult> {
        Some(match name {
            "todo" => self.todo(arguments),
            "clarify" => self.clarify.call(arguments).await,
            "session_search" => self.search.call(arguments).await,
            "memory" => self.memory.as_ref()?.call(arguments).await,
            _ => return None,
        })
    }

    /// The todo text re-injected after compaction.
    #[must_use]
    pub fn todo_reinjection(&self) -> Option<String> {
        self.todo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reinjection()
    }

    fn todo(&self, arguments: Value) -> ToolResult {
        let mut list = self.todo.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = list.clone();
        match next.call(arguments) {
            Ok((changed, reply)) => {
                if changed {
                    if let Err(err) = next.save(&self.store, self.session) {
                        return ToolResult::error(format!(
                            "todo could not be saved: {}",
                            err.message()
                        ));
                    }
                    *list = next;
                }
                ToolResult::ok(reply.to_string())
            }
            Err(message) => ToolResult::error(message),
        }
    }
}
