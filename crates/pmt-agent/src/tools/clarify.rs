//! The `clarify` tool (spec: agent-turn-loop, clarify tool contract).
//!
//! One call sends one `clarify.request` covering all its questions to the
//! session's surface and waits up to `agent.clarify_timeout_secs` (0 =
//! unlimited) for the answers. The question travels over the same
//! human-only [`Channel`] as approval prompts, so nothing a tool returns
//! can answer it. Sessions with no interactive channel do not offer the
//! tool ([`pmt_tools::Selection::without_interactive`]); a call that still
//! reaches one is told to proceed on its own judgment.

use std::time::Duration;

use pmt_core::human::{Channel, Surface, Unanswered};
use pmt_tools::agent_level::{
    CLARIFY_MAX_CHOICE_CHARS, CLARIFY_MAX_CHOICES, CLARIFY_MAX_QUESTIONS,
};
use pmt_tools::{Status, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One question as the surface shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// The question text.
    pub question: String,
    /// Offered choices (at most 4); the surface always adds `Other`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Several choices may be picked.
    #[serde(default)]
    pub multi: bool,
}

/// The `clarify.request` payload: every question of one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClarifyRequest {
    /// The questions, in order.
    pub questions: Vec<Question>,
    /// Always true: surfaces render a free-text `Other` option.
    pub allow_other: bool,
}

/// How one question was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    /// The user answered.
    Answered,
    /// The user skipped this question.
    Skipped,
}

/// The answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Answered or skipped.
    pub status: AnswerStatus,
    /// The picked choices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Free text (the `Other` option).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The surface's reply: one answer per question, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarifyAnswer {
    /// The answers.
    pub answers: Vec<Answer>,
}

/// The session's end of `clarify`.
#[derive(Debug)]
pub struct Clarifier {
    channel: Channel<ClarifyRequest, ClarifyAnswer>,
    timeout: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    questions: Vec<Question>,
}

impl Clarifier {
    /// A clarifier asking an interactive surface, waiting `timeout_secs`
    /// (`agent.clarify_timeout_secs`; 0 = no limit) for its answer.
    #[must_use]
    pub fn attached(timeout_secs: u64) -> (Self, Surface<ClarifyRequest, ClarifyAnswer>) {
        let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));
        let (channel, surface) = Channel::attached(timeout.unwrap_or(Duration::MAX));
        (Self { channel, timeout }, surface)
    }

    /// A clarifier of a session no human attends (cron, API, `-z`).
    #[must_use]
    pub fn unattended() -> Self {
        Self {
            channel: Channel::unattended(),
            timeout: None,
        }
    }

    /// Runs one `clarify` call.
    pub async fn call(&self, arguments: Value) -> ToolResult {
        let questions = match validated(arguments) {
            Ok(questions) => questions,
            Err(message) => return ToolResult::error(message),
        };
        let request = ClarifyRequest {
            questions: questions.clone(),
            allow_other: true,
        };
        match self.channel.ask(request).await {
            Ok(reply) => ToolResult::ok(answered(&questions, reply).to_string()),
            Err(Unanswered::TimedOut(_)) => {
                let minutes = self.timeout.map_or(0, |t| t.as_secs().div_ceil(60));
                ToolResult::text(
                    Status::Ok,
                    format!("[user did not respond within {minutes}m]"),
                )
            }
            Err(Unanswered::Dismissed) => {
                ToolResult::ok("[user dismissed the questions without answering]")
            }
            Err(why @ (Unanswered::Unattended | Unanswered::Detached)) => ToolResult::error(
                format!("clarify cannot reach the user ({why}); proceed with your best judgment"),
            ),
        }
    }
}

/// The questions of a valid call, or the message refusing it (nothing is
/// asked then).
fn validated(arguments: Value) -> Result<Vec<Question>, String> {
    let args: Args =
        serde_json::from_value(arguments).map_err(|err| format!("invalid clarify call: {err}"))?;
    if args.questions.is_empty() {
        return Err("questions needs at least 1 item.".to_owned());
    }
    if args.questions.len() > CLARIFY_MAX_QUESTIONS {
        return Err(format!(
            "questions supports at most {CLARIFY_MAX_QUESTIONS} items."
        ));
    }
    for question in &args.questions {
        if question.choices.len() > CLARIFY_MAX_CHOICES {
            return Err(format!(
                "choices supports at most {CLARIFY_MAX_CHOICES} items."
            ));
        }
        if question
            .choices
            .iter()
            .any(|c| c.chars().count() > CLARIFY_MAX_CHOICE_CHARS)
        {
            return Err(format!(
                "each choice supports at most {CLARIFY_MAX_CHOICE_CHARS} characters."
            ));
        }
    }
    Ok(args.questions)
}

/// Each question with its answer; a question the surface left out counts
/// as skipped.
fn answered(questions: &[Question], reply: ClarifyAnswer) -> Value {
    let mut answers = reply.answers.into_iter();
    let entries: Vec<Value> = questions
        .iter()
        .map(|q| {
            let answer = answers.next().unwrap_or(Answer {
                status: AnswerStatus::Skipped,
                choices: Vec::new(),
                text: None,
            });
            json!({"question": q.question, "status": answer.status, "choices": answer.choices, "text": answer.text})
        })
        .collect();
    json!({"answers": entries})
}
