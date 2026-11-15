//! Kanban lifecycle observer hooks (spec: plugin-system, Kanban lifecycle
//! observer hooks). A [`HookQueue`] observes every board this process
//! opens: each committed transaction's events become hook payloads
//! (`kanban_task_claimed`, `kanban_task_completed`, `kanban_task_blocked`,
//! `on_kanban_worker_spawned`, `on_kanban_worker_exited`,
//! `on_kanban_worker_stale_claim`, `on_kanban_task_updated`), queued, and
//! fired by [`HookQueue::flush`] once the caller's board work is done, so
//! no callback runs inside a transaction or holds the board. The
//! dispatcher adds `on_kanban_dispatch_tick` per board after its tick.
//! Return values are ignored.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_kanban::Board;
use pmt_kanban::dispatch::Summary;
use pmt_kanban::model::Event;
use pmt_kanban::observe::Observer;
use serde_json::{Map, Value, json};

/// One queued hook firing.
type Fired = (&'static str, Map<String, Value>);

/// Collects hook payloads from board events until flushed.
pub struct HookQueue {
    profile: String,
    queued: Mutex<Vec<Fired>>,
}

/// Task fields an edit names (values are never reported).
const EDIT_FIELDS: [&str; 5] = ["title", "body", "priority", "reviewer", "review"];

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The names of the fields an `edited`, `assigned` or `model_set` event
/// changed.
fn changed_fields(event: &Event) -> Vec<&'static str> {
    match event.kind.as_str() {
        "assigned" => vec!["assignee"],
        "model_set" => vec!["model", "provider"],
        _ => EDIT_FIELDS
            .into_iter()
            .filter(|f| event.payload.get(*f).is_some())
            .collect(),
    }
}

impl HookQueue {
    /// A queue firing as `scope`'s profile.
    #[must_use]
    pub fn new(scope: &ProfileScope) -> Arc<Self> {
        Arc::new(Self {
            profile: scope.name().to_string(),
            queued: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, hook: &'static str, payload: Map<String, Value>) {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((hook, payload));
    }

    /// The hook and its extra fields for `event`, if it fires one.
    fn hook_of(board: &Board, event: &Event) -> Option<(&'static str, Value)> {
        let p = &event.payload;
        Some(match event.kind.as_str() {
            "claimed" => ("kanban_task_claimed", json!({})),
            "completed" => ("kanban_task_completed", json!({"summary": p["summary"]})),
            "blocked" | "gave_up" => ("kanban_task_blocked", json!({"reason": p["reason"]})),
            "worker_spawned" => {
                let workspace = event
                    .task_id
                    .as_deref()
                    .and_then(|id| board.task(id).ok())
                    .map(|t| board.workspace_dir(&t));
                (
                    "on_kanban_worker_spawned",
                    json!({"pid": p["pid"], "workspace": workspace}),
                )
            }
            "worker_exited" => (
                "on_kanban_worker_exited",
                json!({"pid": p["pid"], "exit_kind": p["exit_kind"], "exit_code": p["exit_code"], "retry": p["retry"]}),
            ),
            "timed_out" => ("on_kanban_worker_stale_claim", json!({})),
            "edited" | "assigned" | "model_set" => (
                "on_kanban_task_updated",
                json!({"fields": changed_fields(event)}),
            ),
            _ => return None,
        })
    }

    /// Queues `on_kanban_dispatch_tick` for each board of a tick
    /// (`summaries`), or `skipped_locked` for each of `locked`.
    pub fn tick(&self, summaries: &[Summary], locked: &[String], dry_run: bool) {
        for s in summaries {
            let outcome = if s.idle() { "idle" } else { "ok" };
            self.push(
                "on_kanban_dispatch_tick",
                object(json!({
                    "board": s.board, "dry_run": dry_run, "outcome": outcome, "profile": self.profile,
                    "counts": {"claimed": s.claimed.len(), "reclaimed": s.reclaimed.len(),
                        "promoted": s.promoted.len(), "blocked": s.blocked.len(),
                        "capped": s.capped.len(), "guarded": s.guarded.len()},
                })),
            );
        }
        for board in locked {
            self.push(
                "on_kanban_dispatch_tick",
                object(json!({"board": board, "dry_run": dry_run, "outcome": "skipped_locked", "profile": self.profile, "counts": {}})),
            );
        }
    }

    /// Fires everything queued under `scope`'s hooks and waits for the
    /// callbacks (run after the board work, never inside it).
    pub fn flush(&self, scope: &ProfileScope) {
        let queued =
            std::mem::take(&mut *self.queued.lock().unwrap_or_else(PoisonError::into_inner));
        if !queued.is_empty() {
            crate::plugins_host::kanban_fired(scope, queued);
        }
    }
}

impl Observer for HookQueue {
    fn observe(&self, board: &Board, events: &[Event]) {
        for event in events {
            let Some((hook, extra)) = Self::hook_of(board, event) else {
                continue;
            };
            let assignee = event
                .task_id
                .as_deref()
                .and_then(|id| board.task(id).ok())
                .and_then(|t| t.assignee);
            let mut payload = object(json!({
                "task_id": event.task_id, "board": board.slug(), "assignee": assignee,
                "run_id": event.run_id, "profile": self.profile,
            }));
            payload.extend(object(extra));
            self.push(hook, payload);
        }
    }
}
