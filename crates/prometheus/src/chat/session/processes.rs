//! How a session's background processes are wired to the rest of the
//! host (spec: execution-environments, Quiet-wait auto-notify; Retained
//! process receipts; Subagent process handoff): receipts in the profile's
//! store, the quiet-wait switch for contexts that can receive a later
//! completion, and the parent session a subagent hands processes to.

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_core::id::SessionId;
use pmt_store::{Origin, ProcessReceipt, Store};
use pmt_tools::environments::Environments;
use pmt_tools::terminal::process::{Adopter, HandedOff, Receipt, Receipts};

/// What a session's terminal registry is built with.
pub struct Wiring {
    /// Quiet-wait auto-notify applies.
    pub auto_notify: bool,
    /// The store-backed receipts of the session's conversation.
    pub receipts: Arc<dyn Receipts>,
    /// The delegating session, for a subagent.
    pub parent: Option<SessionId>,
}

impl Wiring {
    /// The wiring of `session` (its row's origin decides the context).
    #[must_use]
    pub fn of(config: &Config, store: &Arc<Store>, session: SessionId, origin: &Origin) -> Self {
        Self {
            auto_notify: auto_notify(config, origin),
            receipts: Arc::new(StoreReceipts {
                store: Arc::clone(store),
                session,
            }),
            parent: match origin {
                Origin::Subagent { parent, .. } => Some(*parent),
                _ => None,
            },
        }
    }

    /// Lets a subagent's terminal hand processes to its parent's.
    pub fn bind(&self, terminal: &pmt_tools::terminal::Terminal) {
        if let Some(parent) = self.parent {
            terminal
                .processes()
                .set_adopter(Arc::new(ParentTerminal(parent.to_string())));
        }
    }
}

/// Whether quiet-wait auto-notify may switch on in a session of `origin`:
/// never where no later completion can arrive (cron runs, subagents) or
/// where background notifications are off.
#[must_use]
pub fn auto_notify(config: &Config, origin: &Origin) -> bool {
    use pmt_config::schema::BackgroundNotifications;
    if !config.terminal.process_wait_auto_notify {
        return false;
    }
    match origin {
        Origin::Cron { .. } | Origin::Subagent { .. } => false,
        Origin::Gateway { .. } => {
            config.gateway.notifications.background != BackgroundNotifications::Off
        }
        Origin::None => true,
    }
}

/// Receipts kept in the profile's `state.db` for one session's
/// conversation.
struct StoreReceipts {
    store: Arc<Store>,
    session: SessionId,
}

impl Receipts for StoreReceipts {
    fn keep(&self, receipt: Receipt) {
        let row = ProcessReceipt {
            id: receipt.process_id,
            command: receipt.command,
            exit_code: receipt.exit_code,
            started_at: receipt.started_at_ms,
            ended_at: receipt.ended_at_ms,
            output: receipt.output,
        };
        if let Err(err) = self.store.keep_process_receipt(self.session, &row) {
            tracing::warn!(process = %row.id, "process receipt not kept: {err}");
        }
    }

    fn matching(&self, prefix: &str) -> Vec<Receipt> {
        self.store
            .process_receipts_matching(self.session, prefix)
            .inspect_err(|err| tracing::warn!("process receipts unreadable: {err}"))
            .unwrap_or_default()
            .into_iter()
            .map(receipt)
            .collect()
    }

    fn all(&self) -> Vec<Receipt> {
        self.store
            .process_receipts(self.session)
            .inspect_err(|err| tracing::warn!("process receipts unreadable: {err}"))
            .unwrap_or_default()
            .into_iter()
            .map(receipt)
            .collect()
    }
}

fn receipt(row: ProcessReceipt) -> Receipt {
    Receipt {
        process_id: row.id,
        command: row.command,
        exit_code: row.exit_code,
        started_at_ms: row.started_at,
        ended_at_ms: row.ended_at,
        output: row.output,
    }
}

/// The parent session's terminal, found in this process when a handoff
/// happens (the parent may have ended since the child started).
struct ParentTerminal(String);

impl Adopter for ParentTerminal {
    fn adopt(&self, process: HandedOff) -> Result<(), String> {
        let terminal = Environments::process()
            .session(&self.0)
            .and_then(|env| env.terminal.clone())
            .ok_or_else(|| format!("the parent session {} has no live terminal", self.0))?;
        terminal.adopt(process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Cron job (no notification switch where no completion can
    /// arrive).
    #[test]
    fn quiet_wait_needs_a_context_that_receives_completions() {
        let mut config = Config::default();
        assert!(auto_notify(&config, &Origin::None));
        let cron = Origin::Cron {
            job_id: "job_1".into(),
        };
        assert!(!auto_notify(&config, &cron));
        let gateway = Origin::Gateway {
            platform: "telegram".into(),
            chat: "1".into(),
            thread: None,
            user: None,
        };
        assert!(auto_notify(&config, &gateway));
        config.gateway.notifications.background = pmt_config::schema::BackgroundNotifications::Off;
        assert!(!auto_notify(&config, &gateway));
        config.terminal.process_wait_auto_notify = false;
        assert!(!auto_notify(&config, &Origin::None));
    }
}
