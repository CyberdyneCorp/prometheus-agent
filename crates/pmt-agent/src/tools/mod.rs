//! The agent-level tools (spec: tool-system, Agent-level tools are owned by
//! the turn loop): their registry entries are `pmt_tools::agent_level`
//! (generic dispatch refuses them); the turn loop runs them here, where the
//! session state they need lives.

pub mod clarify;
pub mod connections;
pub mod cron;
pub mod delegation;
pub mod desktop;
pub mod memory;
pub mod messaging;
pub mod session_search;
pub mod todo;

use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::cancel::CancelToken;
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::ToolResult;
use serde_json::Value;

pub use clarify::Clarifier;
pub use cron::CronPort;
pub use delegation::{CodePort, DelegationPort};
pub use desktop::{DesktopPort, DesktopPorts};
pub use memory::{MemoryPort, Recalled};
pub use messaging::{MessagingPort, MessagingPorts, ReadCheck};
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
    messaging: Option<Arc<dyn MessagingPort>>,
    connections: Option<Arc<pmt_tools::connectors::manage::Manager>>,
    delegation: Option<Arc<dyn DelegationPort>>,
    code: Option<Arc<dyn CodePort>>,
    cron: Option<Arc<dyn CronPort>>,
    desktop: Option<Arc<dyn DesktopPort>>,
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
            messaging: None,
            connections: None,
            delegation: None,
            code: None,
            cron: None,
            desktop: None,
        })
    }

    /// Runs `memory` calls through `memory` (pass the same port as
    /// `AgentSetup::memory`).
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryPort>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The session's human-input channel (also asked for MCP server
    /// elicitations).
    #[must_use]
    pub fn clarifier(&self) -> &Clarifier {
        &self.clarify
    }

    /// Sends `send_message` calls through `port` (the daemon's gateway).
    #[must_use]
    pub fn with_messaging(mut self, port: Arc<dyn MessagingPort>) -> Self {
        self.messaging = Some(port);
        self
    }

    /// Runs `manage_connections` through `manager`.
    #[must_use]
    pub fn with_connections(
        mut self,
        manager: Arc<pmt_tools::connectors::manage::Manager>,
    ) -> Self {
        self.connections = Some(manager);
        self
    }

    /// Runs `delegate_task` through `port`.
    #[must_use]
    pub fn with_delegation(mut self, port: Arc<dyn DelegationPort>) -> Self {
        self.delegation = Some(port);
        self
    }

    /// Runs `cronjob` through `port`.
    #[must_use]
    pub fn with_cron(mut self, port: Arc<dyn CronPort>) -> Self {
        self.cron = Some(port);
        self
    }

    /// Runs the desktop UI tools, `setup_beat` and `project` through
    /// `port`.
    #[must_use]
    pub fn with_desktop(mut self, port: Arc<dyn DesktopPort>) -> Self {
        self.desktop = Some(port);
        self
    }

    /// Runs `execute_code` through `port`.
    #[must_use]
    pub fn with_code(mut self, port: Arc<dyn CodePort>) -> Self {
        self.code = Some(port);
        self
    }

    /// Runs `name` when it is an agent-level tool this loop executes, in a
    /// turn whose hard stop is `cancel`; `None` for any other tool (the
    /// generic dispatcher runs it).
    pub async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancelToken,
    ) -> Option<ToolResult> {
        Some(match name {
            "delegate_task" => match &self.delegation {
                Some(port) => port.call(arguments, cancel).await,
                None => ToolResult::error(delegation::NO_DELEGATION),
            },
            "execute_code" => match &self.code {
                Some(port) => port.call(arguments, cancel).await,
                None => ToolResult::error(delegation::NO_CODE),
            },
            "cronjob" => match &self.cron {
                Some(port) => port.call(self.session, arguments).await,
                None => ToolResult::error(cron::NO_CRON),
            },
            name if desktop::handles(name) => match &self.desktop {
                Some(port) => port.call(name, arguments, cancel).await,
                None => desktop::unavailable(name),
            },
            "todo" => self.todo(arguments),
            "clarify" => self.clarify.call(arguments).await,
            "session_search" => self.search.call(arguments).await,
            "memory" => self.memory.as_ref()?.call(arguments).await,
            "send_message" => match &self.messaging {
                Some(port) => port.call(self.session, arguments).await,
                None => ToolResult::error(messaging::NO_GATEWAY),
            },
            pmt_tools::connectors::manage::NAME => match &self.connections {
                Some(manager) => {
                    let card = connections::ClarifyCard::new(&self.clarify);
                    manager.call(&arguments, Some(&card)).await
                }
                None => pmt_tools::connectors::manage::not_set_up(),
            },
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
