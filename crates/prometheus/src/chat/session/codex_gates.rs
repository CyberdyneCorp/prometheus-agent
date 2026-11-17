//! The Codex app-server runtime's approval requests (spec: model-providers,
//! Codex app-server runtime): a command Codex wants to run is checked as a
//! `terminal` call and a file change as a `patch` call by the approval
//! gate of the session the turn belongs to, so the same rules, prompts and
//! `approval.request` apply as to the session's own tools. A request of a
//! session without a registered gate is declined.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use pmt_core::id::SessionId;
use pmt_providers::codex::{RuntimeApprovals, RuntimeAsk};
use pmt_tools::ToolRegistry;
use pmt_tools::approval::CommandGate;
use pmt_tools::dispatch::{ApprovalGate as _, GateRequest, GateVerdict};

/// A session's gate and the registry its asks are checked against.
type Entry = (Weak<CommandGate>, Weak<ToolRegistry>);

/// The gates of a factory's open sessions.
#[derive(Default)]
pub(super) struct CodexGates {
    sessions: Mutex<HashMap<String, Entry>>,
}

impl CodexGates {
    /// Answers session `id`'s Codex approvals with `gate`, finding the
    /// tools they are checked as in `registry`.
    pub(super) fn register(
        &self,
        id: SessionId,
        gate: &Arc<CommandGate>,
        registry: &Arc<ToolRegistry>,
    ) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, (gate, _)| gate.strong_count() > 0);
        sessions.insert(
            id.to_string(),
            (Arc::downgrade(gate), Arc::downgrade(registry)),
        );
    }

    fn of(&self, session: &str) -> Option<(Arc<CommandGate>, Arc<ToolRegistry>)> {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let (gate, registry) = sessions.get(session)?;
        Some((gate.upgrade()?, registry.upgrade()?))
    }
}

impl RuntimeApprovals for CodexGates {
    fn ask(&self, ask: RuntimeAsk) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let Some((gate, registry)) = ask.session.as_deref().and_then(|s| self.of(s)) else {
                return false;
            };
            let Ok(tool) = registry.get(ask.tool) else {
                return false;
            };
            let session = ask.session.unwrap_or_default();
            let verdict = gate
                .check(GateRequest {
                    session_id: &session,
                    call_id: &ask.call_id,
                    tool,
                    arguments: &ask.arguments,
                    escalation: None,
                    guarded: None,
                    tainted: None,
                })
                .await;
            // A partial review of a Codex change cannot be applied: only
            // an outright allow lets it run.
            verdict == GateVerdict::Allow
        })
    }
}
