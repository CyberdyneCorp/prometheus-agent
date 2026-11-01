//! The turn loop's view of the tools: the session's frozen schemas and a
//! batch runner.
//!
//! [`SessionTools`] is the production port: every call goes through the
//! session's [`Dispatcher`] (schema checks, hooks, approvals, budgets),
//! planned as one dependency graph by resource claims; the agent-level
//! tools (`todo`, `clarify`, `session_search`) execute in the loop's
//! [`AgentTools`] at their place in that plan.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pmt_core::cancel::CancelToken;
use pmt_obs::trajectory::payload::{BatchEdge, BatchPlanned};
use pmt_providers::request::Tool;
use pmt_tools::dispatch::{AgentLoopCalls, DispatchObserver, SeamFuture};
use pmt_tools::{Dispatcher, Effect, ToolCall, ToolResult};
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

/// One response's calls, run.
#[derive(Debug, Clone, Default)]
pub struct ToolBatch {
    /// One outcome per call, in call order.
    pub outcomes: Vec<ToolOutcome>,
    /// The plan the batch ran by (`tool.batch_planned`), when planned.
    pub plan: Option<BatchPlanned>,
}

/// The tools as the turn loop runs them.
pub trait ToolPort: Send + Sync {
    /// The session's tool schemas in request order.
    fn definitions(&self) -> Vec<Tool>;

    /// Whether repeating a call of `name` changes nothing (read-only or
    /// declared idempotent): the no-progress guardrail counts it.
    fn idempotent(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Runs one response's calls; outcomes come back in call order.
    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch>;
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
    agent: Option<Arc<dyn AgentLoopCalls>>,
    dispatcher: Arc<Dispatcher>,
    timings: Arc<Timings>,
}

impl SessionTools {
    /// Agent-level tools from `agent`, every call through `dispatcher`,
    /// whose observer reports into `timings`.
    #[must_use]
    pub fn new(
        agent: Option<Arc<AgentTools>>,
        dispatcher: Arc<Dispatcher>,
        timings: Arc<Timings>,
    ) -> Self {
        Self {
            agent: agent.map(|tools| Arc::new(LoopTools(tools)) as Arc<dyn AgentLoopCalls>),
            dispatcher,
            timings,
        }
    }

    async fn run_all(&self, calls: &[RequestedCall], cancel: &CancelToken) -> ToolBatch {
        let batch: Vec<ToolCall> = calls
            .iter()
            .map(|c| ToolCall::new(&c.id, &c.name, &c.arguments))
            .collect();
        let plan = self.dispatcher.plan(&batch);
        let planned = BatchPlanned {
            calls: calls.iter().map(|c| c.id.clone()).collect(),
            edges: plan
                .edges()
                .map(|(from, to)| BatchEdge {
                    call: calls[to].id.clone(),
                    after: calls[from].id.clone(),
                })
                .collect(),
        };
        let started = Instant::now();
        let results = self
            .dispatcher
            .run_planned(batch, &plan, cancel, self.agent.clone())
            .await;
        let batch_ms = elapsed_ms(started);
        let outcomes = calls
            .iter()
            .zip(results)
            .map(|(call, result)| ToolOutcome {
                result,
                duration_ms: self.timings.take(&call.id).unwrap_or(batch_ms),
            })
            .collect();
        ToolBatch {
            outcomes,
            plan: Some(planned),
        }
    }
}

/// The agent-level tools as the dispatcher calls them.
struct LoopTools(Arc<AgentTools>);

impl AgentLoopCalls for LoopTools {
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        _cancel: &'a CancelToken,
    ) -> SeamFuture<'a, ToolResult> {
        // The dispatcher races the call against the cancel token.
        Box::pin(async move {
            self.0.call(name, arguments).await.unwrap_or_else(|| {
                ToolResult::error(format!("{name} must be handled by the agent loop"))
            })
        })
    }
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

    fn idempotent(&self, name: &str) -> bool {
        self.dispatcher
            .surface()
            .get(name)
            .is_some_and(|t| t.descriptor.idempotent || t.descriptor.effect == Effect::None)
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(self.run_all(calls, cancel))
    }
}
