//! The plugin `subagents` interface in the binary (spec:
//! subagent-delegation, Subagent lifecycle interface for plugins). The
//! host already checked the `subagents` grant; the call then runs through
//! the delegation of the session it is made for, so a plugin launch meets
//! exactly the checks a `delegate_task` spawn of that session meets.
//!
//! Each open session registers its delegation here ([`register`]) for as
//! long as it lives; a call made outside a session has nothing to launch
//! from and is refused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use pmt_delegation::host::Delegator;
use pmt_delegation::host::plugin::LifecycleError;
use pmt_plugins::host::ports::{
    PortFuture, SubagentAnswer, SubagentFailure, SubagentRequest, SubagentsPort,
};
use serde_json::Value;

type Sessions = Mutex<HashMap<String, Weak<Delegator>>>;

fn sessions() -> &'static Sessions {
    static SESSIONS: OnceLock<Sessions> = OnceLock::new();
    SESSIONS.get_or_init(Sessions::default)
}

/// Keeps a session's delegation reachable for plugin calls until dropped.
pub struct Registration {
    session: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        sessions()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

/// Makes `delegator` the delegation of plugin calls made for `session`.
#[must_use]
pub fn register(session: &str, delegator: &Arc<Delegator>) -> Registration {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(session.to_owned(), Arc::downgrade(delegator));
    Registration {
        session: session.to_owned(),
    }
}

fn delegator(session: &str) -> Option<Arc<Delegator>> {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session)
        .and_then(Weak::upgrade)
}

/// The [`SubagentsPort`] of every profile's plugins.
pub struct SessionsSubagents;

impl SubagentsPort for SessionsSubagents {
    fn call<'a>(
        &'a self,
        session: &'a str,
        plugin: &'a str,
        request: SubagentRequest,
    ) -> PortFuture<'a, Result<SubagentAnswer, SubagentFailure>> {
        Box::pin(async move {
            let Some(delegation) = delegator(session) else {
                return Err(SubagentFailure::Refused(format!(
                    "delegation.session_closed: session {session} cannot delegate"
                )));
            };
            let answer = run(&delegation, request).await.map_err(failure);
            if let Ok(SubagentAnswer::Handle(_)) = &answer {
                tracing::info!(plugin, session, "plugin launched a subagent");
            }
            answer
        })
    }
}

async fn run(
    delegation: &Arc<Delegator>,
    request: SubagentRequest,
) -> Result<SubagentAnswer, LifecycleError> {
    let text = |value: Option<Value>| value.map(|v| v.to_string());
    Ok(match request {
        SubagentRequest::Launch { task } => {
            let task: Value = serde_json::from_str(&task).map_err(|e| {
                LifecycleError::Refused(pmt_delegation::Refusal::new(
                    &pmt_core::error::codes::DELEGATION_INVALID_TASK,
                    format!("The task is not JSON: {e}"),
                ))
            })?;
            SubagentAnswer::Handle(delegation.launch(&task).await?)
        }
        SubagentRequest::Status { handle } => {
            SubagentAnswer::State(delegation.status(&handle)?.as_str())
        }
        SubagentRequest::Wait { handle, timeout } => {
            SubagentAnswer::Result(text(delegation.wait(&handle, timeout).await?))
        }
        SubagentRequest::Result { handle } => {
            SubagentAnswer::Result(text(delegation.result(&handle)?))
        }
        SubagentRequest::Steer { handle, message } => {
            delegation.steer(&handle, &message)?;
            SubagentAnswer::Done
        }
        SubagentRequest::Cancel { handle } => {
            delegation.cancel(&handle)?;
            SubagentAnswer::Done
        }
    })
}

fn failure(err: LifecycleError) -> SubagentFailure {
    match err {
        LifecycleError::UnknownHandle => SubagentFailure::UnknownHandle,
        LifecycleError::Refused(refusal) => {
            SubagentFailure::Refused(format!("{}: {}", refusal.code.code, refusal.message))
        }
    }
}
