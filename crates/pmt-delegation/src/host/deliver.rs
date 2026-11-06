//! Results into the parent's context and durable background completions
//! (spec: subagent-delegation, Summary budget protects the parent
//! context; Durable background completions).
//!
//! A background call's completion is recorded in the store before it is
//! handed to the session, claimed exactly once ([`Store::claim_completion`])
//! and put back when the session could not take it, up to 8 attempts or
//! 48 hours. On restart, children left running are finalised as
//! `interrupted` ([`recover`]) and the owed completions are delivered
//! when their session is next loaded ([`deliver_pending`]).

use std::path::Path;
use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_store::Store;
use serde_json::{Value, json};

use super::{Completions, Delegator};
use crate::result::{ExitReason, ResultError, Status, SubagentResult};
use crate::summary;

/// The input type a completion arrives as.
pub const COMPLETION_TYPE: &str = "delegation_completed";

impl Delegator {
    /// `results` as inserted into the parent's context: summaries capped
    /// with the full text kept under `summary_ref`.
    pub(super) fn for_context(&self, results: &[SubagentResult], tasks: usize) -> Vec<Value> {
        let cap = summary::cap(
            self.ctx.config.max_summary_chars,
            (self.ctx.context_left)(),
            tasks,
        );
        results
            .iter()
            .map(|result| capped(result, cap, &self.ctx.spill_dir))
            .collect()
    }

    /// Delivers a finished background call once every child ended.
    pub(super) async fn complete(
        &self,
        delegation_id: &str,
        results: &[SubagentResult],
        tasks: usize,
        sink: &Arc<dyn Completions>,
        label: Option<&str>,
    ) {
        if self.suspended() {
            return;
        }
        let text = completion_text(delegation_id, &self.for_context(results, tasks), label);
        deliver_one(&self.ctx.store, delegation_id, text, sink.as_ref()).await;
    }
}

/// One result with its summary capped at `cap` (full text kept in
/// `spill_dir`).
#[must_use]
pub fn capped(result: &SubagentResult, cap: Option<usize>, spill_dir: &Path) -> Value {
    let mut result = result.clone();
    if let Some(short) = cap.and_then(|cap| summary::truncate(&result.summary, cap)) {
        match summary::keep_full(spill_dir, &result.subagent_id, &result.summary) {
            Ok(path) => result.summary_ref = Some(path.to_string_lossy().into_owned()),
            Err(err) => tracing::warn!(error = %err, "full subagent summary not kept"),
        }
        result.summary = short;
    }
    serde_json::to_value(result).unwrap_or(Value::Null)
}

/// The `delegation_completed` input text of `results`, headed by
/// `label` when the dispatch named one (`/review`).
#[must_use]
pub fn completion_text(delegation_id: &str, results: &[Value], label: Option<&str>) -> String {
    let mut body = json!({
        "type": COMPLETION_TYPE,
        "delegation_id": delegation_id,
        "results": results,
    });
    let heading = match label {
        Some(label) => {
            body["label"] = label.into();
            format!("{label}: background subagents finished")
        }
        None => "Background subagents finished".to_owned(),
    };
    format!(
        "[{COMPLETION_TYPE} {delegation_id}] {heading}. Their results:\n{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    )
}

/// Claims and delivers completion `delegation_id`; puts it back when the
/// session could not take it.
pub async fn deliver_one(store: &Store, delegation_id: &str, text: String, sink: &dyn Completions) {
    match store.claim_completion(delegation_id) {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            tracing::warn!(error = %err, delegation_id, "completion not claimed");
            return;
        }
    }
    if sink.deliver(delegation_id, text).await {
        return;
    }
    match store.release_completion(delegation_id) {
        Ok(true) => tracing::info!(
            delegation_id,
            "completion not delivered; it will be retried"
        ),
        Ok(false) => tracing::warn!(
            delegation_id,
            "completion dropped after its last delivery attempt"
        ),
        Err(err) => tracing::warn!(error = %err, delegation_id, "completion not released"),
    }
}

/// Startup recovery: children a stopped process left `queued` or
/// `running` end as `interrupted`, and completions older than 48 hours are
/// dropped with a WARN. Run once per profile store before any spawn.
pub fn recover(store: &Store) {
    let interrupted = store.interrupt_unfinished(&|row| {
        let index = usize::try_from(row.task_index).unwrap_or_default();
        let mut result = SubagentResult::ended(
            &row.id,
            index,
            Status::Interrupted,
            ExitReason::Interrupted,
            Some(ResultError {
                code: "delegation.interrupted".to_owned(),
                message: "The process running this subagent stopped before it finished.".to_owned(),
            }),
        );
        result.trajectory_ref.clone_from(&row.session_id);
        serde_json::to_string(&result).unwrap_or_default()
    });
    match interrupted {
        Ok(rows) if !rows.is_empty() => {
            tracing::info!(
                children = rows.len(),
                "subagents interrupted by the restart"
            );
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "unfinished subagents not recovered"),
    }
    match store.drop_stale_completions() {
        Ok(dropped) => {
            for id in dropped {
                tracing::warn!(delegation_id = %id, "undelivered delegation completion older than 48 hours dropped");
            }
        }
        Err(err) => tracing::warn!(error = %err, "stale completions not swept"),
    }
}

/// Delivers every completion owed to `root`'s conversation (a session was
/// loaded or resumed), each exactly once. `cap` and `spill_dir` cap the
/// summaries as at dispatch.
pub async fn deliver_pending(
    store: &Store,
    root: SessionId,
    cap: Option<usize>,
    spill_dir: &Path,
    sink: &dyn Completions,
) {
    let pending = match store.pending_completions(root) {
        Ok(pending) => pending,
        Err(err) => {
            tracing::warn!(error = %err, "pending completions not read");
            return;
        }
    };
    for delegation in pending {
        let Ok(rows) = store.subagents_of(&delegation.id) else {
            continue;
        };
        let results: Vec<Value> = rows
            .iter()
            .filter_map(|row| row.result.as_deref())
            .filter_map(|json| serde_json::from_str::<SubagentResult>(json).ok())
            .map(|result| capped(&result, cap, spill_dir))
            .collect();
        let text = completion_text(&delegation.id, &results, None);
        deliver_one(store, &delegation.id, text, sink).await;
    }
}
