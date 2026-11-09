//! The `subagents` host import (spec: subagent-delegation, Subagent
//! lifecycle interface for plugins). Every call needs the `subagents`
//! grant and a session to act in; the port runs it through that session's
//! delegation, which owns validation, grant narrowing and handle checks.

use std::time::Duration;

use pmt_core::capability::{Capability, PluginRight};

use super::bindings::prometheus::subagents::lifecycle::{self, Failure, State};
use super::imports::HostState;
use super::imports::block_on;
use super::ports::{SubagentAnswer, SubagentFailure, SubagentRequest};

impl HostState {
    /// Runs `request` for the calling session after the grant check.
    fn subagents(&self, request: SubagentRequest) -> Result<SubagentAnswer, Failure> {
        let right = Capability::Plugin {
            right: PluginRight::Subagents,
            target: None,
        };
        self.require(&right)
            .map_err(|_| Failure::Denied("capability_denied: subagents".to_owned()))?;
        let Some(session) = self.call.session_id.clone() else {
            return Err(Failure::Refused(
                "delegation.session_closed: subagents need a session; this call runs outside one"
                    .to_owned(),
            ));
        };
        let port = std::sync::Arc::clone(&self.ports().subagents);
        let plugin = self.ctx.plugin.clone();
        match block_on(async move { port.call(&session, &plugin, request).await }) {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(SubagentFailure::UnknownHandle)) => Err(Failure::UnknownHandle),
            Ok(Err(SubagentFailure::Refused(why))) => Err(Failure::Refused(why)),
            Err(_) => Err(Failure::Refused(
                "delegation.session_closed: the host call failed".to_owned(),
            )),
        }
    }
}

fn unexpected() -> Failure {
    Failure::Refused("delegation.invalid_task: unexpected answer from the host".to_owned())
}

fn state(name: &str) -> Result<State, Failure> {
    Ok(match name {
        "queued" => State::Queued,
        "running" => State::Running,
        "completed" => State::Completed,
        "failed" => State::Failed,
        "interrupted" => State::Interrupted,
        "stalled" => State::Stalled,
        "cancel_requested" => State::CancelRequested,
        "cancelled" => State::Cancelled,
        _ => return Err(unexpected()),
    })
}

impl lifecycle::Host for HostState {
    fn launch(&mut self, task: String) -> Result<String, Failure> {
        match self.subagents(SubagentRequest::Launch { task })? {
            SubagentAnswer::Handle(handle) => Ok(handle),
            _ => Err(unexpected()),
        }
    }

    fn status(&mut self, handle: String) -> Result<State, Failure> {
        match self.subagents(SubagentRequest::Status { handle })? {
            SubagentAnswer::State(name) => state(name),
            _ => Err(unexpected()),
        }
    }

    fn wait(&mut self, handle: String, timeout_ms: u32) -> Result<Option<String>, Failure> {
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        match self.subagents(SubagentRequest::Wait { handle, timeout })? {
            SubagentAnswer::Result(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }

    fn steer(&mut self, handle: String, message: String) -> Result<(), Failure> {
        self.subagents(SubagentRequest::Steer { handle, message })
            .map(|_| ())
    }

    fn cancel(&mut self, handle: String) -> Result<(), Failure> {
        self.subagents(SubagentRequest::Cancel { handle })
            .map(|_| ())
    }

    fn result(&mut self, handle: String) -> Result<Option<String>, Failure> {
        match self.subagents(SubagentRequest::Result { handle })? {
            SubagentAnswer::Result(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }
}
