//! Questions the session asks the person and the line that answers them:
//! command approvals, clarify questions, memory approvals and the
//! `once / always / cancel` confirmation of destructive commands.

use std::fmt::Write as _;

use pmt_agent::tools::clarify::{Answer, AnswerStatus, ClarifyAnswer, ClarifyRequest};
use pmt_agent::turn::model::BoxFuture;
use pmt_core::human::Prompt;
use pmt_memory::session::Approver;
use pmt_tools::approval::{ApprovalRequest, Decision};
use tokio::sync::{mpsc, oneshot};

/// A memory change waiting for the person.
pub struct MemoryAsk {
    /// What would change.
    pub preview: String,
    /// The answer.
    pub reply: oneshot::Sender<bool>,
}

/// The memory approver of an interactive session: asks the REPL.
pub struct MemoryApprover {
    asks: mpsc::UnboundedSender<MemoryAsk>,
}

impl MemoryApprover {
    /// An approver sending its questions to `asks`.
    #[must_use]
    pub fn new(asks: mpsc::UnboundedSender<MemoryAsk>) -> Self {
        Self { asks }
    }
}

impl Approver for MemoryApprover {
    fn ask<'a>(&'a self, preview: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let ask = MemoryAsk {
                preview: preview.to_owned(),
                reply,
            };
            self.asks.send(ask).ok()?;
            answer.await.ok()
        })
    }
}

/// What the next line answers.
pub(super) enum Pending {
    /// A command approval.
    Approval(Prompt<ApprovalRequest, Decision>),
    /// Clarify questions; `answers` so far.
    Clarify {
        /// The prompt.
        prompt: Prompt<ClarifyRequest, ClarifyAnswer>,
        /// Answers given so far, one per question.
        answers: Vec<Answer>,
    },
    /// A memory change.
    Memory(oneshot::Sender<bool>),
    /// A destructive command waiting for `once / always / cancel`.
    Confirm {
        /// The command line.
        line: String,
    },
}

impl Pending {
    /// The prompt shown while this is pending.
    pub(super) fn prompt(&self) -> String {
        match self {
            Self::Approval(_) => "approve? [o]nce [s]ession [a]lways [d]eny › ".to_owned(),
            Self::Clarify { .. } => "answer › ".to_owned(),
            Self::Memory(_) => "save to memory? [y/n] › ".to_owned(),
            Self::Confirm { .. } => "once / always / cancel › ".to_owned(),
        }
    }

    /// Gives up: denies, skips or cancels.
    pub(super) fn cancel(self) {
        match self {
            Self::Approval(prompt) => prompt.respond(Decision::Deny),
            Self::Clarify { prompt, .. } => drop(prompt),
            Self::Memory(reply) => {
                let _ = reply.send(false);
            }
            Self::Confirm { .. } => {}
        }
    }
}

/// The question text of an approval.
pub(super) fn approval_question(request: &ApprovalRequest) -> String {
    let subject = request.subject.as_deref().unwrap_or(&request.tool);
    format!(
        "{} wants to run: {subject}\n  why: {}",
        request.tool,
        request.summary()
    )
}

/// The decision a line gives, if it is one.
pub(super) fn decision(line: &str) -> Option<Decision> {
    Some(match line.trim().to_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Decision::Once,
        "s" | "session" => Decision::Session,
        "a" | "always" => Decision::Always,
        "d" | "deny" | "n" | "no" => Decision::Deny,
        _ => return None,
    })
}

/// The text of clarify question `index`.
pub(super) fn clarify_question(request: &ClarifyRequest, index: usize) -> Option<String> {
    let question = request.questions.get(index)?;
    let mut text = question.question.clone();
    for (n, choice) in question.choices.iter().enumerate() {
        let _ = write!(text, "\n  {}. {choice}", n + 1);
    }
    Some(text)
}

/// The answer a line gives to clarify question `index` (a choice number,
/// a choice's text or free text; empty skips).
pub(super) fn clarify_answer(request: &ClarifyRequest, index: usize, line: &str) -> Answer {
    let line = line.trim();
    if line.is_empty() {
        return Answer {
            status: AnswerStatus::Skipped,
            choices: Vec::new(),
            text: None,
        };
    }
    let choices = request
        .questions
        .get(index)
        .map(|q| q.choices.as_slice())
        .unwrap_or_default();
    let picked = line
        .parse::<usize>()
        .ok()
        .and_then(|n| choices.get(n.checked_sub(1)?))
        .or_else(|| choices.iter().find(|c| c.eq_ignore_ascii_case(line)));
    match picked {
        Some(choice) => Answer {
            status: AnswerStatus::Answered,
            choices: vec![choice.clone()],
            text: None,
        },
        None => Answer {
            status: AnswerStatus::Answered,
            choices: Vec::new(),
            text: Some(line.to_owned()),
        },
    }
}

/// What a confirmation line chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Confirmation {
    /// Run it this time.
    Once,
    /// Run it and stop asking.
    Always,
    /// Leave everything as it is.
    Cancel,
}

/// The choice of a confirmation line (anything else cancels).
pub(super) fn confirmation(line: &str) -> Confirmation {
    match line.trim().to_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Confirmation::Once,
        "a" | "always" => Confirmation::Always,
        _ => Confirmation::Cancel,
    }
}

#[cfg(test)]
mod tests {
    use pmt_agent::tools::clarify::Question;

    use super::*;

    #[test]
    fn lines_map_to_answers() {
        assert_eq!(decision(" S "), Some(Decision::Session));
        assert_eq!(decision("maybe"), None);
        assert_eq!(confirmation("always"), Confirmation::Always);
        assert_eq!(confirmation(""), Confirmation::Cancel);
        let request = ClarifyRequest {
            questions: vec![Question {
                question: "Which?".into(),
                choices: vec!["red".into(), "blue".into()],
                multi: false,
            }],
            allow_other: true,
        };
        assert_eq!(clarify_answer(&request, 0, "2").choices, ["blue"]);
        assert_eq!(clarify_answer(&request, 0, "RED").choices, ["red"]);
        assert_eq!(
            clarify_answer(&request, 0, "green").text.as_deref(),
            Some("green")
        );
        assert_eq!(
            clarify_answer(&request, 0, "").status,
            AnswerStatus::Skipped
        );
    }
}
