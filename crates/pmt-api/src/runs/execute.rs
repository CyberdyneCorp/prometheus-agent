//! A run's background work: it waits while an `unknown` run holds its
//! session, runs the turn with approvals and clarifications as pending
//! controls, turns every [`TurnEvent`] into a run event and records the
//! terminal status, usage and runtime (the pair that served the turn
//! after any fallback). The concurrent-run permit is held until the
//! turn's work exits.

use std::sync::Arc;

use pmt_store::api::runs::RunUpdate;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Pending, Run, redacted_preview};
use crate::Shared;
use crate::admission::Finish;
use crate::host::ApiTurn;
use crate::turn::{self, Answering, Outcome, TurnEvent};

pub(super) async fn execute(shared: Arc<Shared>, run: Arc<Run>, turn: ApiTurn, finish: Finish) {
    wait_for_unknown(&shared, &run).await;
    if run.state.lock().expect("run poisoned").done {
        return;
    }
    let started = run.record(&RunUpdate {
        status: Some("running".to_owned()),
        ..RunUpdate::default()
    });
    let requested = started
        .and_then(|r| r.requested)
        .and_then(|r| serde_json::from_str::<Value>(&r).ok())
        .unwrap_or(Value::Null);
    run.events
        .push("run.started", json!({"session_id": run.session}));
    let interim = shared.host.api_scope(&run.profile).is_none_or(|s| {
        s.config()
            .gateway
            .display
            .for_platform("api_server")
            .interim_messages
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let work = turn::run(&shared, &run.profile, turn, &tx, Answering::Controls);
    tokio::pin!(work);
    let mut relay = Relay {
        shared: &shared,
        run: &run,
        interim,
    };
    // A stop answered before the turn installed its interrupt (the run
    // is `running` a moment before) would be lost: it is sent again once
    // the turn shows it runs.
    let mut stop_resent = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut work => break outcome,
            Some(event) = rx.recv() => {
                relay.event(event);
                if !stop_resent && run.state.lock().expect("run poisoned").stopping {
                    stop_resent = true;
                    if let Err(err) = shared.host.interrupt(&run.profile, &run.session).await {
                        tracing::debug!(run = run.id, "interrupt: {err}");
                    }
                }
            }
        }
    };
    while let Ok(event) = rx.try_recv() {
        relay.event(event);
    }
    record_end(&run, outcome, &requested);
    // The run slot is free only once the turn's work exited.
    drop(finish);
}

/// Waits while an `unknown` run of the same session is unresolved: its
/// lost turn may still be the session's head (spec: Run controls,
/// Unknown run resolved).
async fn wait_for_unknown(shared: &Shared, run: &Run) {
    loop {
        let resolved = shared.runs.resolved.notified();
        let blocked = run
            .store
            .active_api_runs()
            .unwrap_or_default()
            .iter()
            .any(|r| r.session_id == run.session && r.status == "unknown");
        if !blocked {
            return;
        }
        resolved.await;
    }
}

struct Relay<'a> {
    shared: &'a Shared,
    run: &'a Run,
    interim: bool,
}

impl Relay<'_> {
    fn event(&mut self, event: TurnEvent) {
        let events = &self.run.events;
        match event {
            TurnEvent::Delta(text) => events.push("message.delta", json!({"delta": text})),
            TurnEvent::Reasoning(text) => events.push("reasoning.delta", json!({"delta": text})),
            TurnEvent::Commentary(text) if self.interim => {
                events.push("message.interim", json!({"text": text, "already_streamed": true}));
            }
            TurnEvent::ToolStarted { call_id, tool, arguments } => {
                let preview = redacted_preview(self.shared, &self.run.profile, &arguments.to_string());
                events.push("tool.started", json!({"tool": tool, "call_id": call_id, "preview": preview}));
            }
            TurnEvent::ToolCompleted { call_id, tool, ok, duration_ms, output } => {
                let preview = redacted_preview(self.shared, &self.run.profile, &output);
                events.push(
                    "tool.completed",
                    json!({"tool": tool, "call_id": call_id, "duration_ms": duration_ms,
                        "error": !ok, "preview": preview}),
                );
            }
            TurnEvent::SubagentStart(payload) => events.push(
                "subagent.start",
                json!({"delegation_id": payload["delegation_id"], "subagent_id": payload["subagent_id"],
                    "child_session_id": payload["child_session_id"], "goal": payload["goal"], "depth": payload["depth"]}),
            ),
            TurnEvent::SubagentComplete(payload) => events.push(
                "subagent.complete",
                json!({"delegation_id": payload["delegation_id"], "subagent_id": payload["subagent_id"],
                    "status": payload["status"],
                    "summary": payload["summary"], "duration_ms": payload["duration_ms"],
                    "tokens": payload["usage"], "cost": payload["usage"]["cost_usd"],
                    "child_session_id": payload["child_session_id"]}),
            ),
            TurnEvent::Asked(control) => self.asked(&control),
            TurnEvent::Settled(id) => self.settled(&id),
            TurnEvent::Commentary(_) => {}
        }
    }

    fn asked(&self, control: &turn::Control) {
        let (generation, status) = {
            let mut state = self.run.state.lock().expect("run poisoned");
            state.generation += 1;
            let generation = state.generation;
            state.pending.push(Pending {
                control: control.clone(),
                generation,
            });
            let status = if state.stopping {
                "stopping"
            } else if control.kind == "approval" {
                "waiting_for_approval"
            } else {
                "waiting_for_clarification"
            };
            (generation, status)
        };
        self.run.record(&RunUpdate {
            status: Some(status.to_owned()),
            execution_generation: Some(generation),
            ..RunUpdate::default()
        });
        let mut data = control.params.clone();
        data["request_id"] = control.id.clone().into();
        data["execution_generation"] = generation.into();
        let name = if control.kind == "approval" {
            "approval.request"
        } else {
            "clarify.request"
        };
        self.run.events.push(name, data);
    }

    fn settled(&self, id: &str) {
        let resume = {
            let mut state = self.run.state.lock().expect("run poisoned");
            let before = state.pending.len();
            state.pending.retain(|p| p.control.id != id);
            before != state.pending.len()
                && state.pending.is_empty()
                && !state.stopping
                && !state.done
        };
        if resume {
            self.run.record(&RunUpdate {
                status: Some("running".to_owned()),
                ..RunUpdate::default()
            });
        }
    }
}

/// Records the terminal status and pushes the terminal event.
fn record_end(run: &Run, outcome: Result<Outcome, pmt_core::Error>, requested: &Value) {
    let stopping = {
        let mut state = run.state.lock().expect("run poisoned");
        if state.done {
            return;
        }
        state.done = true;
        state.pending.clear();
        state.stopping
    };
    let (status, output, error, usage, runtime) = match outcome {
        Ok(outcome) => {
            let status = match outcome.status() {
                "completed" | "incomplete" => "completed",
                "cancelled" => "cancelled",
                _ if stopping => "cancelled",
                _ => "failed",
            };
            let error = (status != "completed").then(|| outcome.error_text());
            let runtime = outcome
                .runtime
                .as_ref()
                .map(|r| crate::model::runtime(r, requested));
            let mut usage = outcome.openai_usage();
            if outcome.status() == "incomplete" {
                usage["error_code"] = "iteration_budget_exhausted".into();
            }
            (
                status,
                outcome.final_response.clone(),
                error,
                usage,
                runtime,
            )
        }
        Err(err) => (
            "failed",
            None,
            Some(err.message().to_owned()),
            json!({}),
            None,
        ),
    };
    run.record(&RunUpdate {
        status: Some(status.to_owned()),
        output: output.clone(),
        usage: Some(usage.to_string()),
        runtime: runtime.as_ref().map(Value::to_string),
        error: error.clone(),
        ..RunUpdate::default()
    });
    let event = format!("run.{status}");
    run.events.push(
        &event,
        json!({"status": status, "output": output, "error": error, "usage": usage, "runtime": runtime}),
    );
}
