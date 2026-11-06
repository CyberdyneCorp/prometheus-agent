//! The plugin `subagents` lifecycle interface (spec: subagent-delegation,
//! Subagent lifecycle interface for plugins): `launch`, `status`, `wait`,
//! `steer`, `cancel` and `result` over the same spawn path as
//! `delegate_task`.
//!
//! A launch is one task, validated, routed, narrowed and admitted exactly
//! like a `delegate_task` spawn of the calling session, but always run in
//! the background and owed to the plugin: its result never enters the
//! conversation. The plugin gets a signed handle; every later call
//! verifies it and checks that the child belongs to the calling session's
//! root conversation, so a forged, altered or foreign handle answers
//! [`LifecycleError::UnknownHandle`] exactly like one that expired.

use std::sync::Arc;
use std::time::Duration;

use pmt_core::cancel::CancelToken;
use serde_json::{Value, json};

use super::Delegator;
use super::spawn::Mode;
use crate::Refusal;
use crate::contract;
use crate::handles::{self, Claims, HandleState};
use crate::result::Status;
use crate::tree::{Entry, Registry, State};

/// Why a lifecycle call did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// The handle does not name a child of this conversation (forged,
    /// altered, foreign or expired).
    UnknownHandle,
    /// The launch or action was refused.
    Refused(Refusal),
}

impl From<Refusal> for LifecycleError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl Delegator {
    /// Starts one child for a plugin: `task` is one `delegate_task` task
    /// object. Answers the child's handle.
    ///
    /// # Errors
    ///
    /// Every refusal a `delegate_task` spawn of this session would meet
    /// (invalid task, paused, depth limit, closed, capacity, router,
    /// broadening).
    pub async fn launch(self: &Arc<Self>, task: &Value) -> Result<String, LifecycleError> {
        let arguments = json!({ "tasks": [task] });
        let call = contract::parse(&arguments, 1, &self.router_params())?;
        let (planned, admissions) = self.admit(&call).await?;
        let delegation_id = crate::ids::delegation_id();
        let children = self.register(
            &delegation_id,
            &call,
            planned,
            admissions,
            Mode::Plugin,
            &CancelToken::new(),
        )?;
        let entry = Arc::clone(&children.first().ok_or_else(empty_launch)?.entry);
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.run_all(children).await;
        });
        Ok(handles::issue(&Claims {
            subagent_id: entry.subagent_id.clone(),
            root_session_id: self.ctx.root.to_string(),
            created_ms: jiff::Timestamp::now().as_millisecond(),
        }))
    }

    /// The child `handle` names, when it belongs to this session's root
    /// conversation and is still kept.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`].
    pub fn child(&self, handle: &str) -> Result<Arc<Entry>, LifecycleError> {
        let claims = handles::verify(handle).ok_or(LifecycleError::UnknownHandle)?;
        if claims.root_session_id != self.ctx.root.to_string() {
            return Err(LifecycleError::UnknownHandle);
        }
        Registry::process()
            .get(&claims.subagent_id)
            .filter(|e| e.root_session == claims.root_session_id)
            .ok_or(LifecycleError::UnknownHandle)
    }

    /// The state of the child `handle` names.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`].
    pub fn status(&self, handle: &str) -> Result<HandleState, LifecycleError> {
        Ok(state_of(&*self.child(handle)?))
    }

    /// Waits up to `timeout` for the child to end: its result, or `None`
    /// while it runs.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`].
    pub async fn wait(
        &self,
        handle: &str,
        timeout: Duration,
    ) -> Result<Option<Value>, LifecycleError> {
        let entry = self.child(handle)?;
        Ok(tokio::time::timeout(timeout, entry.wait())
            .await
            .ok()
            .map(|result| serde_json::to_value(result).unwrap_or(Value::Null)))
    }

    /// The child's result once it ended (immutable from then on).
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`].
    pub fn result(&self, handle: &str) -> Result<Option<Value>, LifecycleError> {
        Ok(match self.child(handle)?.state() {
            State::Ended(result) => Some(serde_json::to_value(*result).unwrap_or(Value::Null)),
            State::Queued | State::Running => None,
        })
    }

    /// Queues `message` for the child's next iteration.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`], or a refusal for an empty message
    /// or a child that already ended.
    pub fn steer(&self, handle: &str, message: &str) -> Result<(), LifecycleError> {
        let entry = self.child(handle)?;
        super::control::steer_entry(&entry, Some(message))?;
        Ok(())
    }

    /// Asks the child to stop; it reports `cancel_requested`, then
    /// `cancelled`.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::UnknownHandle`].
    pub fn cancel(&self, handle: &str) -> Result<(), LifecycleError> {
        self.child(handle)?.request_cancel();
        Ok(())
    }
}

/// The lifecycle state of `entry`.
#[must_use]
pub fn state_of(entry: &Entry) -> HandleState {
    let cancelled = entry.cancelled();
    match entry.state() {
        State::Queued | State::Running if cancelled => HandleState::CancelRequested,
        State::Queued => HandleState::Queued,
        State::Running => HandleState::Running,
        State::Ended(result) => match result.status {
            Status::Interrupted if cancelled => HandleState::Cancelled,
            Status::Interrupted => HandleState::Interrupted,
            Status::Completed => HandleState::Completed,
            Status::Stalled => HandleState::Stalled,
            Status::Failed | Status::Refused => HandleState::Failed,
        },
    }
}

fn empty_launch() -> LifecycleError {
    LifecycleError::Refused(Refusal::new(
        &pmt_core::error::codes::DELEGATION_INVALID_TASK,
        "The launch built no child.",
    ))
}

#[cfg(test)]
mod tests {
    use pmt_agent::turn::Interrupt;

    use super::*;
    use crate::result::{ExitReason, SubagentResult};
    use crate::tree::NewEntry;

    fn entry(registry: &Registry, id: &str) -> Arc<Entry> {
        registry.add(
            NewEntry {
                subagent_id: id.to_owned(),
                delegation_id: "dg-1".to_owned(),
                task_index: 0,
                goal: "g".to_owned(),
                depth: 1,
                parent_session: "ses".to_owned(),
                parent_conversation: "ses".to_owned(),
                root_session: "ses".to_owned(),
                background: true,
                queued: true,
            },
            Interrupt::default(),
        )
    }

    fn end(entry: &Entry, status: Status) {
        entry.finish(&SubagentResult::ended(
            &entry.subagent_id,
            0,
            status,
            ExitReason::Interrupted,
            None,
        ));
    }

    /// Every lifecycle state is reachable.
    #[test]
    fn every_state_is_reachable() {
        let registry = Registry::default();
        let queued = entry(&registry, "sa-q");
        assert_eq!(state_of(&queued), HandleState::Queued);
        queued.started("ses-child", "mock/mock");
        assert_eq!(state_of(&queued), HandleState::Running);
        queued.request_cancel();
        assert_eq!(state_of(&queued), HandleState::CancelRequested);
        end(&queued, Status::Interrupted);
        assert_eq!(state_of(&queued), HandleState::Cancelled);
        for (status, state) in [
            (Status::Completed, HandleState::Completed),
            (Status::Failed, HandleState::Failed),
            (Status::Refused, HandleState::Failed),
            (Status::Interrupted, HandleState::Interrupted),
            (Status::Stalled, HandleState::Stalled),
        ] {
            let e = entry(&registry, &format!("sa-{}", status.as_str()));
            end(&e, status);
            assert_eq!(state_of(&e), state, "{status:?}");
        }
    }
}
