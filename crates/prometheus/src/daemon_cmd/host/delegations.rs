//! Background delegation completions of daemon sessions (spec:
//! subagent-delegation, Background and synchronous dispatch; Durable
//! background completions).
//!
//! An interactive client session (TUI, desktop, attached line chat) gets a
//! finished background delegation as a new turn whose input is the
//! `delegation_completed` text, queued behind any running turn; a gateway
//! chat gets it injected into its conversation, which runs it as a turn
//! and replies in the chat. A session not loaded when its children end
//! receives the completion when it is next loaded: completions are owed
//! in the store and delivered exactly once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use pmt_agent::turn::TurnInput;
use pmt_agent::turn::model::BoxFuture;
use pmt_core::id::SessionId;
use pmt_delegation::host::Completions;
use pmt_delegation::host::deliver::{self, COMPLETION_TYPE};
use pmt_gateway::Link;
use pmt_gateway::messaging::plugin_ops::Injection;
use serde_json::json;
use tracing::Instrument as _;

use super::send_tool::GatewayCell;
use super::turns::Queued;
use super::{DaemonHost, Loaded, Profile};

/// Delivers to an interactive client session as a queued turn.
pub(super) struct ClientInbox {
    loaded: OnceLock<Weak<Loaded>>,
    profile: Weak<Profile>,
    id: SessionId,
    link: Link,
    turns: Arc<AtomicUsize>,
    slots: pmt_gateway::slots::Slots,
}

impl ClientInbox {
    pub(super) fn new(
        host: &DaemonHost,
        profile: &Arc<Profile>,
        id: SessionId,
        link: Link,
    ) -> Arc<Self> {
        Arc::new(Self {
            loaded: OnceLock::new(),
            profile: Arc::downgrade(profile),
            id,
            link,
            turns: Arc::clone(&host.turns),
            slots: host.slots.clone(),
        })
    }

    /// Binds the loaded session (it exists only after its clients do).
    pub(super) fn bind(&self, session: &Arc<Loaded>) {
        let _ = self.loaded.set(Arc::downgrade(session));
    }

    fn enqueue(&self, delegation_id: &str, text: String) -> bool {
        let (Some(session), Some(profile)) = (
            self.loaded.get().and_then(Weak::upgrade),
            self.profile.upgrade(),
        ) else {
            return false;
        };
        let mut input = TurnInput::text(text);
        input.author = Some(json!({"type": COMPLETION_TYPE, "delegation_id": delegation_id}));
        let job = Queued {
            input,
            turn_id: pmt_agent::turn::TurnId::new().to_string(),
            run_budget: None,
            link: self.link.clone(),
        };
        {
            let mut queue = session.queue.lock().expect("queue poisoned");
            if session.busy.swap(true, Ordering::AcqRel) {
                queue.push_back(job);
                return true;
            }
        }
        let started = DaemonHost::start_with(
            Arc::clone(&self.turns),
            self.slots.clone(),
            profile,
            self.id,
            session,
            job,
        );
        started
            .inspect_err(
                |err| tracing::warn!(error = %err, "delegation completion turn not started"),
            )
            .is_ok()
    }
}

impl Completions for ClientInbox {
    fn deliver<'a>(&'a self, delegation_id: &'a str, text: String) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.enqueue(delegation_id, text) })
    }
}

/// Delivers to a gateway chat's conversation, which runs it as a turn.
pub(super) struct ChatInbox {
    pub(super) gateway: GatewayCell,
    pub(super) profile: String,
    pub(super) session: String,
}

impl Completions for ChatInbox {
    fn deliver<'a>(&'a self, _delegation_id: &'a str, text: String) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(gateway) = self.gateway.get() else {
                return false;
            };
            gateway
                .inject(
                    &self.profile,
                    &Injection::Session(self.session.clone()),
                    &text,
                )
                .await
                .inspect_err(|why| tracing::info!(why, "delegation completion not injected yet"))
                .is_ok()
        })
    }
}

/// Delivers what the session's conversation is owed (completions of
/// background delegations that ended while it was not loaded, or before a
/// restart).
pub(super) fn deliver_owed(profile: &Profile, session: &Loaded, inbox: Arc<dyn Completions>) {
    let store = Arc::clone(&profile.store);
    let scope = session.live.scope.clone();
    let root = match store.session(session.live.id) {
        Ok(row) => row.root_session_id,
        Err(_) => return,
    };
    let spill = pmt_tools::spillover::session_dir(scope.dir(), &session.live.id.to_string());
    let cap = pmt_delegation::summary::cap(scope.config().delegation.max_summary_chars, None, 1);
    tokio::spawn(
        async move {
            deliver::deliver_pending(&store, root, cap, &spill, inbox.as_ref()).await;
        }
        .in_current_span(),
    );
}
