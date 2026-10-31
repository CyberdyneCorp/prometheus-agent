//! The turn loop's view of the tools: the session's frozen schemas and a
//! batch runner.
//!
//! [`SessionTools`] is the production port: agent-level tools (`todo`,
//! `clarify`, `session_search`) run here, every other call goes through the
//! session's [`Dispatcher`] (schema checks, approvals, budgets, parallel
//! batches by resource claims).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pmt_core::cancel::CancelToken;
use pmt_providers::request::Tool;
use pmt_tools::dispatch::DispatchObserver;
use pmt_tools::{Dispatcher, ToolCall, ToolResult};
use serde_json::Value;

use super::model::{BoxFuture, RequestedCall};
use crate::tools::AgentTools;

/// One finished call.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// The result the model sees.
    pub result: ToolResult,
    /// Wall time of the call.
    pub duration_ms: u64,
}

/// The tools as the turn loop runs them.
pub trait ToolPort: Send + Sync {
    /// The session's tool schemas in request order.
    fn definitions(&self) -> Vec<Tool>;

    /// Runs one response's calls; results come back in call order.
    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Vec<ToolOutcome>>;
}

/// Per-call wall times reported by the dispatcher (pass one as the
/// dispatcher's observer and to [`SessionTools::new`]).
#[derive(Debug, Default)]
pub struct Timings {
    durations: Mutex<HashMap<String, u64>>,
}

impl Timings {
    fn take(&self, call_id: &str) -> Option<u64> {
        self.durations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id)
    }
}

impl DispatchObserver for Timings {
    fn tool_call(&self, _: &str, _: &str, _: &str) {}

    fn tool_result(&self, call_id: &str, _: &str, _: &ToolResult, duration_ms: u64) {
        self.durations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_owned(), duration_ms);
    }
}

/// The production port.
pub struct SessionTools {
    agent: Option<Arc<AgentTools>>,
    dispatcher: Arc<Dispatcher>,
    timings: Arc<Timings>,
}

impl SessionTools {
    /// Agent-level tools from `agent`, the rest through `dispatcher`, whose
    /// observer reports into `timings`.
    #[must_use]
    pub fn new(
        agent: Option<Arc<AgentTools>>,
        dispatcher: Arc<Dispatcher>,
        timings: Arc<Timings>,
    ) -> Self {
        Self {
            agent,
            dispatcher,
            timings,
        }
    }

    async fn run_all(&self, calls: &[RequestedCall], cancel: &CancelToken) -> Vec<ToolOutcome> {
        let mut outcomes: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
        let mut generic = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            match self.agent_level(call).await {
                Some(outcome) => outcomes[i] = Some(outcome),
                None => generic.push(i),
            }
        }
        let batch: Vec<ToolCall> = generic
            .iter()
            .map(|&i| ToolCall::new(&calls[i].id, &calls[i].name, &calls[i].arguments))
            .collect();
        let started = Instant::now();
        let results = self.dispatcher.run_batch(batch, cancel).await;
        let batch_ms = elapsed_ms(started);
        for (&i, result) in generic.iter().zip(results) {
            let duration_ms = self.timings.take(&calls[i].id).unwrap_or(batch_ms);
            outcomes[i] = Some(ToolOutcome {
                result,
                duration_ms,
            });
        }
        outcomes
            .into_iter()
            .map(|o| {
                o.unwrap_or_else(|| ToolOutcome {
                    result: ToolResult::error("no result"),
                    duration_ms: 0,
                })
            })
            .collect()
    }

    async fn agent_level(&self, call: &RequestedCall) -> Option<ToolOutcome> {
        let agent = self.agent.as_ref()?;
        // A tool the session was not offered goes to the dispatcher, which
        // refuses it like any unknown name.
        if !is_agent_level(&call.name) || self.dispatcher.surface().get(&call.name).is_none() {
            return None;
        }
        let started = Instant::now();
        let result = match serde_json::from_str::<Value>(&call.arguments) {
            Ok(arguments) => agent.call(&call.name, arguments).await?,
            Err(err) => ToolResult::error(format!("arguments are not valid JSON: {err}")),
        };
        Some(ToolOutcome {
            result,
            duration_ms: elapsed_ms(started),
        })
    }
}

fn is_agent_level(name: &str) -> bool {
    matches!(name, "todo" | "clarify" | "session_search")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl ToolPort for SessionTools {
    fn definitions(&self) -> Vec<Tool> {
        self.dispatcher
            .surface()
            .definitions()
            .iter()
            .map(|d| Tool {
                name: d.name.clone(),
                description: d.description.clone(),
                parameters: d.parameters.clone(),
            })
            .collect()
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Vec<ToolOutcome>> {
        Box::pin(self.run_all(calls, cancel))
    }
}
