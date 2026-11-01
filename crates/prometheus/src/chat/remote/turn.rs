//! One turn in the daemon: `prompt.submit`, then the session's events
//! until `turn.finished`, answering the turn's approval and clarify
//! requests and turning Ctrl-C into `session.interrupt`.

use std::io::Write as _;

use pmt_core::error::{Error, codes};
use pmt_rpc::RpcError;
use pmt_rpc::client::{ServerCall, error_of};
use pmt_rpc::contracts::{
    Envelope, PromptSubmit, SessionInterrupt, SessionRef, SubmitParams, TurnFinished,
};
use serde_json::{Value, json};

use super::Attached;
use super::line::Input;
use super::render::View;
use crate::chat::signals::Signals;

/// Who answers the turn's requests.
pub(super) enum Asker<'a> {
    /// Nobody: approvals follow `approvals.single_query_mode` (`approve`
    /// when true) and other questions stay unanswered.
    OneShot {
        /// `approvals.single_query_mode = "approve"`.
        approve: bool,
    },
    /// The person at the line interface.
    Line(&'a mut Input),
}

/// How a turn ended.
#[derive(Debug, Default)]
pub(super) struct Outcome {
    /// The turn's end (absent when the connection was lost first).
    pub finished: Option<TurnFinished>,
    /// The status of a SIGINT/SIGTERM that interrupted it.
    pub signal: Option<u8>,
    /// A one-shot run met an approval it could not give.
    pub approval_needed: bool,
}

/// A request waiting for the person's next line.
enum Asking {
    Approval {
        id: String,
    },
    Clarify {
        id: String,
        questions: Vec<Value>,
        answers: Vec<Value>,
    },
}

impl Asking {
    fn id(&self) -> &str {
        match self {
            Self::Approval { id } | Self::Clarify { id, .. } => id,
        }
    }
}

/// Runs `text` as a turn of the attached session.
///
/// # Errors
///
/// The `prompt.submit` refusal (`session.busy`, `budget.exceeded`...), or
/// `rpc.connection` when the connection ends before the turn does.
pub(super) async fn run(
    attached: &mut Attached,
    text: String,
    asker: &mut Asker<'_>,
    view: View,
    signals: &mut Signals,
) -> Result<Outcome, Error> {
    let params = SubmitParams {
        session_id: attached.session.clone(),
        text,
    };
    let turn_id = attached
        .client
        .call_typed::<PromptSubmit>(&params)
        .await
        .map_err(|e| error_of(&e))?
        .turn_id;
    let mut outcome = Outcome::default();
    let mut asking: Option<Asking> = None;
    loop {
        tokio::select! {
            event = attached.inbox.events.recv() => {
                let Some(event) = event else {
                    return Err(lost(attached.daemon_pid));
                };
                if let Some(finished) = on_event(&event, &turn_id, view, &mut asking) {
                    outcome.finished = Some(finished);
                    return Ok(outcome);
                }
            }
            Some(call) = attached.inbox.requests.recv() => {
                asking = ask(attached, call, asker, &mut outcome).await;
            }
            code = signals.next(), if outcome.signal.is_none() => {
                outcome.signal = Some(code);
                interrupt(attached).await;
            }
            line = next_line(asker) => {
                asking = on_line(attached, asking, line, asker);
            }
        }
    }
}

/// The next typed line, for the line interface (never for a one-shot).
async fn next_line(asker: &mut Asker<'_>) -> Option<String> {
    match asker {
        Asker::Line(input) if !input.closed => input.next().await,
        _ => std::future::pending().await,
    }
}

fn lost(daemon_pid: u32) -> Error {
    Error::new(
        &codes::RPC_CONNECTION,
        format!("the connection to the gateway daemon (pid {daemon_pid}) ended during the turn"),
    )
    .with_hint("see 'prometheus daemon status'; the turn's transcript is kept by the daemon")
}

/// Shows a session event; returns the turn's end when this is it.
fn on_event(
    event: &Envelope,
    turn_id: &str,
    view: View,
    asking: &mut Option<Asking>,
) -> Option<TurnFinished> {
    match event.event.as_str() {
        "session.event" => view.event(&event.data),
        "turn.finished" => {
            let finished: TurnFinished = serde_json::from_value(event.data.clone()).ok()?;
            return (finished.turn_id == turn_id).then_some(finished);
        }
        "request.cancel" | "request.resolved" => {
            let id = event.data["id"].as_str().unwrap_or_default();
            if asking.as_ref().is_some_and(|a| a.id() == id) {
                *asking = None;
                println!("(the question was answered elsewhere or timed out)");
            }
        }
        _ => {}
    }
    None
}

async fn interrupt(attached: &Attached) {
    let params = SessionRef {
        session_id: attached.session.clone(),
    };
    let _ = attached
        .client
        .call_typed::<SessionInterrupt>(&params)
        .await;
}

fn unanswered() -> RpcError {
    RpcError::from_error(&Error::new(
        &codes::CHAT_APPROVAL_REQUIRED,
        "nobody can answer this request here",
    ))
}

/// Answers a one-shot request at once, or shows a line-interface one and
/// returns it to wait for the person's line.
async fn ask(
    attached: &Attached,
    call: ServerCall,
    asker: &mut Asker<'_>,
    outcome: &mut Outcome,
) -> Option<Asking> {
    let client = &attached.client;
    match (asker, call.method.as_str()) {
        (Asker::OneShot { approve: true }, "approval.request") => {
            client.respond(&call.id, Ok(json!({"decision": "once"})));
            None
        }
        (Asker::OneShot { .. }, "approval.request") => {
            client.respond(&call.id, Ok(json!({"decision": "deny"})));
            outcome.approval_needed = true;
            interrupt(attached).await;
            None
        }
        (Asker::Line(_), "approval.request") => {
            show_approval(&call.params);
            Some(Asking::Approval { id: call.id })
        }
        (Asker::Line(_), "clarify.request") => {
            let questions = call.params["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            show_question(questions.first());
            Some(Asking::Clarify {
                id: call.id,
                questions,
                answers: Vec::new(),
            })
        }
        _ => {
            client.respond(&call.id, Err(unanswered()));
            None
        }
    }
}

/// A typed line answers the open request, or waits for the next prompt.
fn on_line(
    attached: &Attached,
    asking: Option<Asking>,
    line: Option<String>,
    asker: &mut Asker<'_>,
) -> Option<Asking> {
    let Asker::Line(input) = asker else {
        return asking;
    };
    let Some(line) = line else {
        input.closed = true;
        if let Some(open) = asking {
            attached.client.respond(open.id(), Err(unanswered()));
        }
        return None;
    };
    match asking {
        None => {
            input.queue.push_back(line);
            None
        }
        Some(Asking::Approval { id }) => {
            if let Some(decision) = decision(&line) {
                attached
                    .client
                    .respond(&id, Ok(json!({ "decision": decision })));
                None
            } else {
                prompt("allow? [o]nce, [s]ession, [a]lways, [d]eny: ");
                Some(Asking::Approval { id })
            }
        }
        Some(Asking::Clarify {
            id,
            questions,
            mut answers,
        }) => {
            let question = &questions[answers.len()];
            answers.push(clarify_answer(question, &line));
            if answers.len() < questions.len() {
                show_question(questions.get(answers.len()));
                return Some(Asking::Clarify {
                    id,
                    questions,
                    answers,
                });
            }
            attached
                .client
                .respond(&id, Ok(json!({ "answers": answers })));
            None
        }
    }
}

fn prompt(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "{text}");
    let _ = out.flush();
}

fn show_approval(params: &Value) {
    let tool = params["tool"].as_str().unwrap_or("tool");
    let subject = params["subject"].as_str().unwrap_or_default();
    println!("approval needed: {tool} {subject}");
    for finding in params["findings"].as_array().into_iter().flatten() {
        println!(
            "  {}: {} ({})",
            finding["detector"].as_str().unwrap_or_default(),
            finding["reason"].as_str().unwrap_or_default(),
            finding["fragment"].as_str().unwrap_or_default()
        );
    }
    prompt("allow? [o]nce, [s]ession, [a]lways, [d]eny: ");
}

fn decision(line: &str) -> Option<&'static str> {
    match line.trim().to_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Some("once"),
        "s" | "session" => Some("session"),
        "a" | "always" => Some("always"),
        "" | "d" | "deny" | "n" | "no" => Some("deny"),
        _ => None,
    }
}

fn show_question(question: Option<&Value>) {
    let Some(question) = question else { return };
    println!("? {}", question["question"].as_str().unwrap_or_default());
    for (n, choice) in question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        println!("  {}. {}", n + 1, choice.as_str().unwrap_or_default());
    }
    prompt("answer (a number, your own text, or empty to skip): ");
}

/// The `clarify` answer a typed line stands for: choice numbers (several,
/// comma-separated, for a multi-choice question), free text, or a skip.
fn clarify_answer(question: &Value, line: &str) -> Value {
    let line = line.trim();
    if line.is_empty() {
        return json!({"status": "skipped"});
    }
    let choices: Vec<&str> = question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let picked: Option<Vec<&str>> = line
        .split(',')
        .map(|n| {
            n.trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| choices.get(n.wrapping_sub(1)).copied())
        })
        .collect();
    match picked {
        Some(picked) if !picked.is_empty() => json!({"status": "answered", "choices": picked}),
        _ => json!({"status": "answered", "text": line}),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{clarify_answer, decision};

    #[test]
    fn typed_lines_become_answers() {
        assert_eq!(decision("o"), Some("once"));
        assert_eq!(decision(""), Some("deny"));
        assert_eq!(decision("maybe"), None);
        let question = json!({"question": "Which?", "choices": ["red", "blue"]});
        assert_eq!(
            clarify_answer(&question, "2"),
            json!({"status": "answered", "choices": ["blue"]})
        );
        assert_eq!(
            clarify_answer(&question, "green"),
            json!({"status": "answered", "text": "green"})
        );
        assert_eq!(clarify_answer(&question, " "), json!({"status": "skipped"}));
    }
}
