//! Questions the session asks the person and the line that answers them:
//! command approvals, clarify questions, memory approvals, a skill's
//! missing secrets (read with the input hidden) and the
//! `once / always / cancel` confirmation of destructive commands (its
//! answers are read by `crate::slash::confirmation`).

use std::fmt::Write as _;

use pmt_agent::tools::clarify::{Answer, AnswerStatus, ClarifyAnswer, ClarifyRequest};
use pmt_agent::turn::model::BoxFuture;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_core::human::Prompt;
use pmt_core::secret::SecretString;
use pmt_i18n::Strings;
use pmt_memory::session::Approver;
use pmt_skills::secrets::SecretPrompt;
use pmt_tools::approval::{ApprovalRequest, Decision, printable};
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

/// A skill secret waiting for the person.
pub struct SecretAsk {
    /// The question.
    pub question: String,
    /// The value (`None`: skipped).
    pub reply: oneshot::Sender<Option<SecretString>>,
}

/// The secret prompt of an interactive session: asks the REPL, which
/// reads the answer with the input hidden (spec: skills, Skill secrets and
/// settings). Questions asked before the REPL listens (a `-s` preload at
/// session start) go unanswered: the secret is reported missing.
pub struct CliSecretPrompt {
    asks: mpsc::UnboundedSender<SecretAsk>,
    listening: AtomicBool,
}

impl CliSecretPrompt {
    /// A prompt sending its questions to `asks`.
    #[must_use]
    pub fn new(asks: mpsc::UnboundedSender<SecretAsk>) -> Self {
        Self {
            asks,
            listening: AtomicBool::new(false),
        }
    }

    /// The REPL answers from now on.
    pub fn listen(&self) {
        self.listening.store(true, Ordering::Relaxed);
    }
}

impl SecretPrompt for CliSecretPrompt {
    /// Blocks (on the blocking pool) until the person answers.
    fn ask(&self, skill: &str, name: &str, prompt: Option<&str>) -> Option<SecretString> {
        if !self.listening.load(Ordering::Relaxed) {
            return None;
        }
        let why = prompt.map(|p| format!(": {p}")).unwrap_or_default();
        let (reply, answer) = oneshot::channel();
        let ask = SecretAsk {
            question: format!("skill {skill} needs secret {name}{why} (input hidden, Enter skips)"),
            reply,
        };
        self.asks.send(ask).ok()?;
        answer.blocking_recv().ok().flatten()
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
    /// A runtime install (`[Y/n]`: Enter installs).
    Install(oneshot::Sender<bool>),
    /// A skill secret, typed hidden.
    Secret(oneshot::Sender<Option<SecretString>>),
    /// A destructive command waiting for `once / always / cancel`.
    Confirm {
        /// The command line.
        line: String,
    },
    /// A turn a crash left unfinished, waiting for `y` to resume it.
    Resume(pmt_agent::turn::Unfinished),
    /// The low-disk prune prompt, waiting for `on`, `off` or `later`.
    Autoprune,
}

impl Pending {
    /// The prompt shown while this is pending, in the profile's language.
    pub(super) fn prompt(&self, strings: &Strings) -> String {
        let key = match self {
            Self::Approval(_) => "approval.prompt",
            Self::Clarify { .. } => "cli.answer_prompt",
            Self::Memory(_) => "cli.memory_prompt",
            Self::Install(_) => "cli.install_prompt",
            Self::Secret(_) => "cli.secret_prompt",
            Self::Confirm { .. } => "cli.confirm_prompt",
            Self::Resume(_) => "cli.resume_prompt",
            Self::Autoprune => "cli.autoprune_prompt",
        };
        strings.get(key).to_owned()
    }

    /// Gives up: denies, skips or cancels.
    pub(super) fn cancel(self) {
        match self {
            Self::Approval(prompt) => prompt.respond(Decision::Deny),
            Self::Clarify { prompt, .. } => drop(prompt),
            Self::Memory(reply) | Self::Install(reply) => {
                let _ = reply.send(false);
            }
            Self::Secret(reply) => {
                let _ = reply.send(None);
            }
            // Left open: the next message closes the crashed turn.
            Self::Confirm { .. } | Self::Resume(_) | Self::Autoprune => {}
        }
    }
}

/// The question text of an approval, in the profile's language.
pub(super) fn approval_question(request: &ApprovalRequest, strings: &Strings) -> String {
    // The subject and findings may come from the model: escaped so a
    // control sequence cannot hide part of what is approved.
    let subject = request.subject.as_deref().unwrap_or(&request.tool);
    let mut text = strings.fmt(
        "approval.question",
        &[
            ("tool", &printable(&request.tool)),
            ("subject", &printable(subject)),
        ],
    );
    let _ = write!(
        text,
        "\n  {}",
        strings.fmt(
            "approval.why",
            &[("reason", &printable(&request.summary()))]
        )
    );
    if let Some(explanation) = &request.explanation {
        for line in explanation.lines() {
            let _ = write!(text, "\n  {}", printable(line));
        }
    }
    text
}

/// What the person sees once an approval is answered (`None` for
/// `explain`, which asks again).
pub(super) fn outcome(decision: Decision, strings: &Strings) -> Option<&str> {
    let key = match decision {
        Decision::Once => "approval.allowed_once",
        Decision::Session => "approval.allowed_session",
        Decision::Always => "approval.allowed_always",
        Decision::Deny => "approval.denied",
        Decision::Explain => return None,
    };
    Some(strings.get(key))
}

/// The decision a line gives, if it is one.
pub(super) fn decision(line: &str) -> Option<Decision> {
    Some(match line.trim().to_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Decision::Once,
        "s" | "session" => Decision::Session,
        "a" | "always" => Decision::Always,
        "d" | "deny" | "n" | "no" => Decision::Deny,
        "e" | "explain" => Decision::Explain,
        _ => return None,
    })
}

/// The answer a line gives to `Install <name> <version> (<size>)? [Y/n]`:
/// an empty line (Enter) or yes installs.
pub(super) fn install_answer(line: &str) -> bool {
    matches!(line.trim().to_lowercase().as_str(), "" | "y" | "yes")
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

#[cfg(test)]
mod tests {
    use pmt_agent::tools::clarify::Question;

    use super::*;

    /// A control sequence in the model's command cannot hide part of it
    /// on the in-process approval prompt.
    #[test]
    fn the_approval_question_shows_control_sequences() {
        let request = ApprovalRequest {
            tool: "terminal".into(),
            call_id: "c1".into(),
            subject: Some("rm -rf ~\u{1b}[2K\rls".into()),
            findings: Vec::new(),
            remember: None,
            explanation: None,
        };
        let text = approval_question(&request, &Strings::english(pmt_i18n::Surface::Core));
        assert!(
            text.starts_with("terminal wants to run: rm -rf ~^[[2K\\rls\n"),
            "{text}"
        );
        assert!(!text.contains('\u{1b}') && !text.contains('\r'), "{text}");
    }

    #[test]
    fn lines_map_to_answers() {
        assert_eq!(decision(" S "), Some(Decision::Session));
        assert_eq!(decision("maybe"), None);
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
