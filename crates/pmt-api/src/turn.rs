//! One API turn: admitted into its session through the daemon, watched
//! through the hub until its `turn.finished`, and reported as a stream of
//! [`TurnEvent`]s plus the [`Outcome`] every endpoint shapes its answer
//! from. Turns of one session run one at a time (a later request waits
//! behind the running one), so every event of the watched session belongs
//! to the turn that watches it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pmt_gateway::Author;
use pmt_rpc::contracts::{ASSISTANT_DELTA, TurnError, TurnFinished};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Shared;
use crate::host::ApiTurn;
use crate::peer::PeerMsg;

/// What happens to the server requests (approvals, clarify) of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answering {
    /// No answer channel: settle every request unanswered (approvals are
    /// decided by `api_server.unattended_approvals` before they get here).
    Nobody,
    /// Expose them as pending controls (Runs).
    Controls,
    /// Leave them to the session's other clients (session chat).
    Others,
}

/// A server request the turn waits on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Control {
    /// The hub request id (`srq-...`), the `request_id` clients answer.
    pub id: String,
    /// `approval` or `clarify`.
    pub kind: &'static str,
    /// The request's parameters.
    pub params: Value,
}

/// Something the turn did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TurnEvent {
    /// Answer text as it streams.
    Delta(String),
    /// Reasoning text.
    Reasoning(String),
    /// Text the model wrote before calling tools.
    Commentary(String),
    /// A tool call started.
    ToolStarted {
        call_id: String,
        tool: String,
        arguments: Value,
    },
    /// A tool call ended.
    ToolCompleted {
        call_id: String,
        tool: String,
        ok: bool,
        duration_ms: Option<u64>,
        output: String,
    },
    /// A delegated child started.
    SubagentStart(Value),
    /// A delegated child ended.
    SubagentComplete(Value),
    /// The turn waits on a request.
    Asked(Control),
    /// A request settled (answered here or elsewhere, timed out).
    Settled(String),
}

/// One item of a turn, in order, for the Responses `output` list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Item {
    Reasoning(String),
    Commentary(String),
    Call {
        call_id: String,
        name: String,
        arguments: String,
    },
    Output {
        call_id: String,
        output: String,
    },
}

/// The provider and model that served the turn's last model call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Runtime {
    pub provider: String,
    pub model: String,
}

/// How a turn ended.
#[derive(Debug, Clone, Default)]
pub(crate) struct Outcome {
    /// The daemon's exit reason (`completed`, `iteration_limit`, ...).
    pub exit_reason: String,
    pub final_response: Option<String>,
    pub error: Option<TurnError>,
    /// The turn's usage (`TurnUsage` JSON).
    pub usage: Value,
    pub runtime: Option<Runtime>,
    pub items: Vec<Item>,
}

impl Outcome {
    /// `completed`, `incomplete`, `cancelled` or `failed`.
    pub(crate) fn status(&self) -> &'static str {
        match self.exit_reason.as_str() {
            "completed" => "completed",
            "iteration_limit" if self.final_response.is_some() => "incomplete",
            "interrupted_by_user" | "interrupted_by_system" => "cancelled",
            _ => "failed",
        }
    }

    /// The error text of a turn that did not complete.
    pub(crate) fn error_text(&self) -> String {
        self.error.as_ref().map_or_else(
            || format!("the turn ended: {}", self.exit_reason),
            |e| e.message.clone(),
        )
    }

    /// OpenAI-style usage with cache read and write tokens.
    pub(crate) fn openai_usage(&self) -> Value {
        let n = |key: &str| self.usage[key].as_u64().unwrap_or(0);
        let prompt = n("input_tokens") + n("cache_read_tokens") + n("cache_write_tokens");
        let completion = n("output_tokens");
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_tokens_details": {"cached_tokens": n("cache_read_tokens")},
            "completion_tokens_details": {"reasoning_tokens": n("reasoning_tokens")},
            "cache_read_tokens": n("cache_read_tokens"),
            "cache_write_tokens": n("cache_write_tokens"),
        })
    }
}

/// Per-session turn order.
#[derive(Default)]
pub(crate) struct SessionLocks {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionLocks {
    pub(crate) fn of(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("locks poisoned");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(session.to_owned()).or_default())
    }
}

/// Runs `turn` in `profile` and reports what it does on `events`.
///
/// # Errors
///
/// The daemon's refusal to admit it (budget, pause, unknown session).
pub(crate) async fn run(
    shared: &Shared,
    profile: &str,
    turn: ApiTurn,
    events: &mpsc::UnboundedSender<TurnEvent>,
    answering: Answering,
) -> Result<Outcome, pmt_core::Error> {
    let session = turn.session_id.clone();
    let lock = shared.locks.of(&session);
    let _turn = lock.lock().await;
    let mut rx = shared.peer.watch(profile, &session);
    let author = Author {
        client_kind: "api".to_owned(),
        client_id: "api".to_owned(),
        principal: profile.to_owned(),
        sent_at: None,
        prefix: None,
        origin: None,
    };
    let link = shared.server.link(&session);
    let submitted = match shared.host.api_submit(profile, turn, author, link).await {
        Ok(submitted) => submitted,
        Err(err) => {
            shared.peer.unwatch(&session);
            return Err(err);
        }
    };
    let mut watch = Watch {
        shared,
        events,
        answering,
        outcome: Outcome::default(),
        text: String::new(),
    };
    while let Some(msg) = rx.recv().await {
        if let Some(finished) = watch.handle(msg)
            && finished.turn_id == submitted.turn_id
        {
            watch.finish(finished);
            break;
        }
    }
    shared.peer.unwatch(&session);
    Ok(watch.outcome)
}

struct Watch<'a> {
    shared: &'a Shared,
    events: &'a mpsc::UnboundedSender<TurnEvent>,
    answering: Answering,
    outcome: Outcome,
    /// The last assistant message's text, until a tool call shows it was
    /// commentary.
    text: String,
}

impl Watch<'_> {
    fn emit(&self, event: TurnEvent) {
        let _ = self.events.send(event);
    }

    fn handle(&mut self, msg: PeerMsg) -> Option<TurnFinished> {
        match msg {
            PeerMsg::Event { kind, payload } => self.event(&kind, &payload),
            PeerMsg::Finished(finished) => return Some(finished),
            PeerMsg::Asked { id, method, params } => self.asked(id, &method, params),
            PeerMsg::Settled { id } => self.emit(TurnEvent::Settled(id)),
        }
        None
    }

    fn event(&mut self, kind: &str, payload: &Value) {
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
        match kind {
            ASSISTANT_DELTA => self.emit(TurnEvent::Delta(text("text"))),
            "assistant.reasoning" => {
                let reasoning = text("text");
                if !reasoning.is_empty() {
                    self.outcome.items.push(Item::Reasoning(reasoning.clone()));
                    self.emit(TurnEvent::Reasoning(reasoning));
                }
            }
            "assistant.message" => self.text = text("text"),
            "tool.call" => self.tool_call(payload),
            "tool.result" => self.tool_result(payload),
            "model.call" => {
                self.outcome.runtime = Some(Runtime {
                    provider: text("provider"),
                    model: text("model"),
                });
            }
            "subagent.spawned" => self.emit(TurnEvent::SubagentStart(payload.clone())),
            "subagent.finished" => self.emit(TurnEvent::SubagentComplete(payload.clone())),
            _ => {}
        }
    }

    fn tool_call(&mut self, payload: &Value) {
        let commentary = std::mem::take(&mut self.text);
        if !commentary.trim().is_empty() {
            self.outcome
                .items
                .push(Item::Commentary(commentary.clone()));
            self.emit(TurnEvent::Commentary(commentary));
        }
        let call_id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        let tool = payload["tool"].as_str().unwrap_or_default().to_owned();
        let arguments = payload["arguments"].clone();
        self.outcome.items.push(Item::Call {
            call_id: call_id.clone(),
            name: tool.clone(),
            arguments: match &arguments {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
        });
        self.emit(TurnEvent::ToolStarted {
            call_id,
            tool,
            arguments,
        });
    }

    fn tool_result(&mut self, payload: &Value) {
        let call_id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        let output = payload["output"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| payload["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or_default();
        self.outcome.items.push(Item::Output {
            call_id: call_id.clone(),
            output: output.clone(),
        });
        self.emit(TurnEvent::ToolCompleted {
            call_id,
            tool: payload["tool"].as_str().unwrap_or_default().to_owned(),
            ok: payload["ok"].as_bool().unwrap_or(false),
            duration_ms: payload["duration_ms"].as_u64(),
            output,
        });
    }

    fn asked(&self, id: String, method: &str, params: Value) {
        let kind = match method {
            "approval.request" => "approval",
            "clarify.request" => "clarify",
            // Secrets and sudo passwords never travel over HTTP.
            _ => {
                self.shared.peer.answer(&id, None);
                return;
            }
        };
        match self.answering {
            Answering::Nobody => {
                let answer = (kind == "approval").then(|| json!({"decision": "deny"}));
                self.shared.peer.answer(&id, answer);
            }
            Answering::Controls => self.emit(TurnEvent::Asked(Control { id, kind, params })),
            Answering::Others => {}
        }
    }

    fn finish(&mut self, finished: TurnFinished) {
        self.outcome.exit_reason = finished.exit_reason;
        self.outcome.final_response = finished.final_response;
        self.outcome.error = finished.error;
        self.outcome.usage = finished.usage.unwrap_or_else(|| json!({}));
    }
}
