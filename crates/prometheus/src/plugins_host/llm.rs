//! `llm.complete` for plugins (spec: plugin-system, Plugin LLM access
//! gate). The host already checked the `llm` grant and any model or
//! provider override; the call then runs on the route of the session it is
//! made for, and its usage row is charged to that session's ledger under
//! the purpose `plugin.<id>`, so it counts toward the session's budgets.
//!
//! Each open session registers its model here ([`register`]) for as long
//! as it lives; a plugin call made outside a session (at load, from an
//! event) has no route and fails.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use pmt_plugins::host::ports::{LlmCall, LlmPort, PortFuture};

/// A session's model, as plugin calls use it.
pub trait SessionModel: Send + Sync {
    /// Runs one plugin completion on the session's route (or the
    /// override the call names) and returns its text.
    fn complete(&self, call: LlmCall) -> PortFuture<'_, Result<String, String>>;
}

type Sessions = Mutex<HashMap<String, Weak<dyn SessionModel>>>;

fn sessions() -> &'static Sessions {
    static SESSIONS: OnceLock<Sessions> = OnceLock::new();
    SESSIONS.get_or_init(Sessions::default)
}

/// Keeps a session's model reachable for plugin calls until dropped.
pub struct Registration {
    session: String,
    _model: Arc<dyn SessionModel>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        sessions()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

/// Makes `model` the route of plugin calls made for `session`.
#[must_use]
pub fn register(session: &str, model: Arc<dyn SessionModel>) -> Registration {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(session.to_owned(), Arc::downgrade(&model));
    Registration {
        session: session.to_owned(),
        _model: model,
    }
}

/// The [`LlmPort`] of every profile's plugins.
pub struct SessionsLlm;

impl LlmPort for SessionsLlm {
    fn complete(&self, call: LlmCall) -> PortFuture<'_, Result<String, String>> {
        let model = call.session_id.as_ref().and_then(|id| {
            sessions()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(id)
                .and_then(Weak::upgrade)
        });
        Box::pin(async move {
            let Some(model) = model else {
                return Err("llm needs a session: this call runs outside one".to_owned());
            };
            model.complete(call).await
        })
    }
}
