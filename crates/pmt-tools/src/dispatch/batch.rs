//! Running one model response's tool calls (spec: tool-system, Parallel
//! and timed execution; Result budget and spillover; agent-turn-loop,
//! Dependency-planned parallel tool batches).
//!
//! The batch is planned ([`BatchPlan`]) and each call starts once the calls
//! it depends on are done, at most `agent.tool_concurrency` at once.
//! Results come back in the original call order, after the per-turn budget
//! spilled the largest ones if the response's results are too big.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use tokio::sync::{Semaphore, watch};

use super::plan::BatchPlan;
use super::{AgentLoopCalls, Dispatcher, Executed, ToolCall};
use crate::budget::TurnResult;
use crate::result::ToolResult;

impl Dispatcher {
    /// The dependency plan of `calls`.
    #[must_use]
    pub fn plan(&self, calls: &[ToolCall]) -> BatchPlan {
        BatchPlan::of(calls, &self.setup.surface)
    }

    /// Runs the calls of one model response; results are in call order.
    /// Agent-level tools answer `<name> must be handled by the agent loop`.
    pub async fn run_batch(&self, calls: Vec<ToolCall>, cancel: &CancelToken) -> Vec<ToolResult> {
        let plan = self.plan(&calls);
        self.run_planned(calls, &plan, cancel, None).await
    }

    /// Runs `calls` by `plan` (from [`Self::plan`] on the same calls), with
    /// the agent-level tools executed by `agent` (the turn loop's).
    pub async fn run_planned(
        &self,
        calls: Vec<ToolCall>,
        plan: &BatchPlan,
        cancel: &CancelToken,
        agent: Option<Arc<dyn AgentLoopCalls>>,
    ) -> Vec<ToolResult> {
        let mut executed = self.scheduled(&calls, plan, cancel, agent).await;
        let mut turn: Vec<TurnResult<'_>> = calls
            .iter()
            .zip(executed.iter_mut())
            .map(|(call, done)| TurnResult {
                call_id: &call.call_id,
                tool: done.tool.as_deref(),
                result: &mut done.result,
            })
            .collect();
        self.setup.budget.apply_turn(&mut turn);
        calls
            .iter()
            .zip(executed)
            .map(|(call, done)| {
                self.setup.observer.tool_result(
                    &call.call_id,
                    &call.name,
                    &done.result,
                    done.duration_ms,
                );
                done.result
            })
            .collect()
    }

    /// Spawns every call; each waits for its dependencies, then for a
    /// concurrency permit.
    async fn scheduled(
        &self,
        calls: &[ToolCall],
        plan: &BatchPlan,
        cancel: &CancelToken,
        agent: Option<Arc<dyn AgentLoopCalls>>,
    ) -> Vec<Executed> {
        let permits = Arc::new(Semaphore::new(self.setup.concurrency.max(1)));
        let (done_tx, done_rx): (Vec<_>, Vec<_>) =
            calls.iter().map(|_| watch::channel(false)).unzip();
        let handles: Vec<_> = calls
            .iter()
            .zip(done_tx)
            .enumerate()
            .map(|(i, (call, done))| {
                let waits: Vec<watch::Receiver<bool>> =
                    plan.deps(i).iter().map(|&d| done_rx[d].clone()).collect();
                let (this, call, cancel) = (self.clone(), call.clone(), cancel.clone());
                let (permits, agent) = (Arc::clone(&permits), agent.clone());
                tokio::spawn(async move {
                    for mut wait in waits {
                        // A dependency whose task died still lets us go.
                        let _ = wait.wait_for(|finished| *finished).await;
                    }
                    let permit = permits.acquire_owned().await.ok();
                    let executed = this.execute(&call, &cancel, agent.as_ref()).await;
                    drop(permit);
                    let _ = done.send(true);
                    executed
                })
            })
            .collect();
        let mut executed = Vec::with_capacity(handles.len());
        for (call, handle) in calls.iter().zip(handles) {
            executed.push(handle.await.unwrap_or_else(|err| Executed {
                tool: self.setup.surface.get(&call.name).cloned(),
                result: ToolResult::error(format!(
                    "[TOOL_ERROR] {} failed: panic: {err}",
                    call.name
                )),
                duration_ms: 0,
            }));
        }
        executed
    }
}
