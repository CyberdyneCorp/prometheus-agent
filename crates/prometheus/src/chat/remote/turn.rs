//! One turn in the daemon: `prompt.submit`, then the session's events
//! until `turn.finished`, answering the turn's approval and clarify
//! requests and turning Ctrl-C into `session.interrupt`.

use pmt_core::error::{Error, codes};
use pmt_i18n::Strings;
use pmt_rpc::RpcError;
use pmt_rpc::client::{ServerCall, error_of};
use pmt_rpc::contracts::{
    ASSISTANT_DELTA, Admission, Envelope, PromptImage, PromptSubmit, SessionInterrupt, SessionRef,
    SubmitParams, TurnFinished,
};
use pmt_tools::approval::printable;
use serde_json::{Value, json};

use super::Attached;
use super::line::{Lines, Typed};
use super::render::{self, Out};
use crate::chat::signals::Signals;

/// A prompt to submit.
#[derive(Debug, Default)]
pub(super) struct Prompt {
    /// Its text.
    pub text: String,
    /// The images that go with it.
    pub images: Vec<PromptImage>,
}

/// Who answers the turn's requests.
pub(super) enum Asker<'a> {
    /// Nobody: approvals follow `approvals.single_query_mode` (`approve`
    /// when true) and other questions stay unanswered.
    OneShot {
        /// `approvals.single_query_mode = "approve"`.
        approve: bool,
    },
    /// The person at the line interface.
    Line(&'a mut Lines),
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

/// Runs `prompt` as a turn of the attached session.
///
/// # Errors
///
/// The `prompt.submit` refusal (`session.busy`, `budget.exceeded`...), or
/// `rpc.connection` when the connection ends before the turn does.
pub(super) async fn run(
    attached: &mut Attached,
    prompt: Prompt,
    asker: &mut Asker<'_>,
    out: &dyn Out,
    signals: &mut Signals,
) -> Result<Outcome, Error> {
    let params = SubmitParams {
        session_id: attached.session.clone(),
        text: prompt.text,
        images: prompt.images,
        files: Vec::new(),
        run_budget_secs: attached.run_budget,
        input_id: None,
        author: None,
    };
    let submitted = attached
        .client
        .call_typed::<PromptSubmit>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    let mut outcome = Outcome::default();
    if submitted.admission == Admission::Handled {
        // Answered without a turn (the `yes` to a `/cron` echo).
        out.notice(submitted.notice.as_deref().unwrap_or_default());
        return Ok(outcome);
    }
    let turn_id = submitted.turn_id;
    let mut asking: Option<Asking> = None;
    loop {
        tokio::select! {
            event = attached.inbox.events.recv() => {
                let Some(event) = event else {
                    return Err(lost(attached.daemon_pid));
                };
                if event.event == "automation.state" {
                    render::automation(&mut attached.automation, &attached.session, &event.data);
                }
                if let Some(finished) = on_event(&event, &turn_id, out, &mut asking) {
                    if let Some(report) = &finished.report {
                        attached.record.remote_turn(report);
                    }
                    outcome.finished = Some(finished);
                    return Ok(outcome);
                }
            }
            Some(call) = attached.inbox.requests.recv() => {
                asking = ask(attached, call, asker, out, &mut outcome).await;
            }
            code = signals.next(), if outcome.signal.is_none() => {
                outcome.signal = Some(code);
                interrupt(attached).await;
            }
            typed = next_typed(asker) => {
                if matches!(typed, Typed::Interrupt) {
                    interrupt(attached).await;
                } else {
                    asking = on_line(attached, asking, typed, asker, out);
                }
            }
        }
    }
}

/// What the person does next at the line interface (never for a
/// one-shot); Ctrl+C there interrupts the turn.
async fn next_typed(asker: &mut Asker<'_>) -> Typed {
    match asker {
        Asker::Line(lines) if !lines.closed => lines.next().await,
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
    out: &dyn Out,
    asking: &mut Option<Asking>,
) -> Option<TurnFinished> {
    match event.event.as_str() {
        "session.event"
            if event.data["type"] == ASSISTANT_DELTA && event.data["turn_id"] == turn_id =>
        {
            out.delta(event.data["payload"]["text"].as_str().unwrap_or_default());
        }
        "session.event" => render::event(out, &event.data),
        "turn.finished" => {
            let finished: TurnFinished = serde_json::from_value(event.data.clone()).ok()?;
            return (finished.turn_id == turn_id).then_some(finished);
        }
        "request.cancel" | "request.resolved" => {
            let id = event.data["id"].as_str().unwrap_or_default();
            if asking.as_ref().is_some_and(|a| a.id() == id) {
                *asking = None;
                out.notice("(the question was answered elsewhere or timed out)");
            }
        }
        _ => {}
    }
    None
}

/// Interrupts whatever turn the session runs (`session.interrupt`).
pub(super) async fn interrupt(attached: &Attached) {
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
    out: &dyn Out,
    outcome: &mut Outcome,
) -> Option<Asking> {
    let client = &attached.client;
    match (asker, call.method.as_str()) {
        // A request that cannot be remembered (a protected instruction
        // file) needs a human: single-query mode never approves it, as
        // the in-process gate does.
        (Asker::OneShot { approve: true }, "approval.request")
            if !call.params["remember"].is_null() =>
        {
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
            out.ask(&approval_text(&call.params, &attached.strings()));
            Some(Asking::Approval { id: call.id })
        }
        (Asker::Line(_), "clarify.request") => {
            let questions = call.params["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            show_question(out, questions.first());
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

impl Attached {
    /// The profile's UI strings now.
    fn strings(&self) -> std::sync::Arc<Strings> {
        pmt_config::language::strings(&self.profile_dir, pmt_i18n::Surface::Core)
    }
}

/// A typed line answers the open request, or waits for the next prompt.
fn on_line(
    attached: &Attached,
    asking: Option<Asking>,
    typed: Typed,
    asker: &mut Asker<'_>,
    out: &dyn Out,
) -> Option<Asking> {
    let Asker::Line(lines) = asker else {
        return asking;
    };
    let line = match typed {
        Typed::Line(line) => line,
        // Composed in the editor while the turn runs: sent after it.
        Typed::Compose(buffer) => {
            lines.queue.push_back(format!("/prompt {buffer}"));
            return asking;
        }
        // Voice records between turns only.
        Typed::Record => return asking,
        Typed::Interrupt | Typed::Eof => {
            lines.closed = true;
            if let Some(open) = asking {
                attached.client.respond(open.id(), Err(unanswered()));
            }
            return None;
        }
    };
    match asking {
        None => {
            lines.queue.push_back(line);
            None
        }
        Some(Asking::Approval { id }) => {
            if let Some(decision) = decision(&line) {
                attached
                    .client
                    .respond(&id, Ok(json!({ "decision": decision })));
                None
            } else {
                out.ask(attached.strings().get("approval.remote_choices"));
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
                show_question(out, questions.get(answers.len()));
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

fn approval_text(params: &Value, strings: &Strings) -> String {
    use std::fmt::Write as _;
    // Every field may come from the model: escaped so a control sequence
    // cannot hide part of what is approved.
    let field = |value: &Value| printable(value.as_str().unwrap_or_default());
    let tool = params["tool"]
        .as_str()
        .map_or_else(|| "tool".to_owned(), printable);
    let mut text = strings.fmt(
        "approval.remote_title",
        &[("tool", &tool), ("subject", &field(&params["subject"]))],
    );
    for finding in params["findings"].as_array().into_iter().flatten() {
        let _ = write!(
            text,
            "\n  {}: {} ({})",
            field(&finding["detector"]),
            field(&finding["reason"]),
            field(&finding["fragment"])
        );
    }
    if let Some(explanation) = params["explanation"].as_str() {
        for line in explanation.lines() {
            let _ = write!(text, "\n  {}", printable(line));
        }
    }
    let _ = write!(text, "\n{}", strings.get("approval.remote_choices"));
    text
}

pub(crate) fn decision(line: &str) -> Option<&'static str> {
    match line.trim().to_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Some("once"),
        "s" | "session" => Some("session"),
        "a" | "always" => Some("always"),
        "" | "d" | "deny" | "n" | "no" => Some("deny"),
        "e" | "explain" => Some("explain"),
        _ => None,
    }
}

fn show_question(out: &dyn Out, question: Option<&Value>) {
    use std::fmt::Write as _;
    let Some(question) = question else { return };
    let mut text = format!("? {}", question["question"].as_str().unwrap_or_default());
    for (n, choice) in question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let _ = write!(
            text,
            "\n  {}. {}",
            n + 1,
            choice.as_str().unwrap_or_default()
        );
    }
    text.push_str("\nanswer (a number, your own text, or empty to skip)");
    out.ask(&text);
}

/// The `clarify` answer a typed line stands for: choice numbers (several,
/// comma-separated, for a multi-choice question), free text, or a skip.
pub(crate) fn clarify_answer(question: &Value, line: &str) -> Value {
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

    use super::{approval_text, clarify_answer, decision};

    /// A control sequence in the model's command cannot hide part of it
    /// on the attached approval prompt.
    #[test]
    fn the_approval_prompt_shows_control_sequences() {
        let text = approval_text(
            &json!({
                "tool": "terminal",
                "subject": "rm -rf ~\u{1b}[2K\rls",
                "findings": [{"detector": "dangerous", "reason": "deletes", "fragment": "rm\u{1b}[1A"}],
            }),
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        );
        assert!(
            text.starts_with("approval needed: terminal rm -rf ~^[[2K\\rls\n"),
            "{text}"
        );
        assert!(text.contains("dangerous: deletes (rm^[[1A)"), "{text}");
        assert!(!text.contains('\u{1b}') && !text.contains('\r'), "{text}");
    }

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
