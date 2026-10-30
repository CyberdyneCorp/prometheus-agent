//! Running one model response's tool calls (spec: tool-system, Parallel
//! and timed execution; Result budget and spillover).
//!
//! The calls run concurrently, at most `agent.tool_concurrency` at once,
//! only when none of them declares a write path, an exclusive resource or
//! `interactive`; otherwise they run one after another in model order.
//! Results come back in the original call order, after the per-turn
//! budget spilled the largest ones if the response's results are too big.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use tokio::sync::Semaphore;

use super::{Dispatcher, Executed, ToolCall};
use crate::budget::TurnResult;
use crate::result::ToolResult;

impl Dispatcher {
    /// Runs the calls of one model response; results are in call order.
    pub async fn run_batch(&self, calls: Vec<ToolCall>, cancel: &CancelToken) -> Vec<ToolResult> {
        let mut executed = if self.parallel(&calls) {
            self.concurrently(&calls, cancel).await
        } else {
            let mut done = Vec::with_capacity(calls.len());
            for call in &calls {
                done.push(self.execute(call, cancel).await);
            }
            done
        };
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

    /// Whether every call may run beside the others. Unknown names fail
    /// without touching anything, so they do not force sequential order.
    fn parallel(&self, calls: &[ToolCall]) -> bool {
        calls.len() > 1
            && calls.iter().all(|call| {
                self.setup
                    .surface
                    .get(&call.name)
                    .is_none_or(|tool| tool.descriptor.claims.parallel_safe())
            })
    }

    async fn concurrently(&self, calls: &[ToolCall], cancel: &CancelToken) -> Vec<Executed> {
        let permits = Arc::new(Semaphore::new(self.setup.concurrency.max(1)));
        let handles: Vec<_> = calls
            .iter()
            .map(|call| {
                let (this, call, cancel) = (self.clone(), call.clone(), cancel.clone());
                let permits = Arc::clone(&permits);
                tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.ok();
                    this.execute(&call, &cancel).await
                })
            })
            .collect();
        let mut done = Vec::with_capacity(handles.len());
        for (call, handle) in calls.iter().zip(handles) {
            done.push(handle.await.unwrap_or_else(|err| Executed {
                tool: self.setup.surface.get(&call.name).cloned(),
                result: ToolResult::error(format!(
                    "[TOOL_ERROR] {} failed: panic: {err}",
                    call.name
                )),
                duration_ms: 0,
            }));
        }
        done
    }
}
