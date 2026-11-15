//! Goal-mode cards (spec: kanban, Goal-mode cards; session-automation,
//! Automation is distinct from kanban and cron). A goal-mode task's worker
//! reuses the session-automation goal judge: the task title and body are
//! the acceptance criteria, and after each turn (and at every handoff)
//! the judge decides `done`, `continue` or `unachievable`. A judge call
//! that fails never blocks a handoff.

use pmt_agent::turn::model::BoxFuture;
use pmt_automation::judge::{self, Verdict};
use pmt_automation::state::Goal;
use pmt_kanban::Task;

/// The worker session's goal judge (its `goal_judge` auxiliary route).
pub trait GoalJudge: Send + Sync {
    /// The judge's reply to `prompt`.
    fn ask(&self, prompt: String) -> BoxFuture<'_, Result<String, String>>;
}

impl GoalJudge for crate::chat::session::judge::RouteJudge {
    fn ask(&self, prompt: String) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move { Self::ask(self, &prompt).await })
    }
}

/// What the judge decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judged {
    /// The acceptance criteria are met.
    Done(String),
    /// Another turn is needed.
    Continue(String),
    /// It cannot be achieved: block the task.
    Unachievable(String),
    /// The judge could not be asked (the handoff is allowed).
    Unreachable(String),
}

impl Judged {
    /// The verdict's name in the log and the `goal_verdict` event.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Done(_) => "done",
            Self::Continue(_) => "continue",
            Self::Unachievable(_) => "unachievable",
            Self::Unreachable(_) => "unreachable",
        }
    }

    /// The judge's reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Done(r) | Self::Continue(r) | Self::Unachievable(r) | Self::Unreachable(r) => r,
        }
    }
}

/// The goal a task's title and body state.
#[must_use]
pub fn goal(task: &Task) -> Goal {
    let mut text = task.title.trim().to_owned();
    if !task.body.trim().is_empty() {
        text.push_str("\n\nAcceptance criteria:\n");
        text.push_str(task.body.trim());
    }
    Goal::new(text, None)
}

/// Asks `judge` whether `response` meets `task`'s goal.
pub async fn verdict(judge: &dyn GoalJudge, task: &Task, response: &str) -> Judged {
    let prompt = judge::prompt(&goal(task), response, &[]);
    match judge.ask(prompt).await {
        Ok(reply) => match judge::parse(&reply) {
            Ok(Verdict::Done(r)) => Judged::Done(r),
            Ok(Verdict::Blocked(r)) => Judged::Unachievable(r),
            Ok(Verdict::Continue(r) | Verdict::Wait { reason: r, .. }) => Judged::Continue(r),
            Err(err) => Judged::Unreachable(err),
        },
        Err(err) => Judged::Unreachable(err),
    }
}

/// The continuation turn after a `continue` verdict.
#[must_use]
pub fn continuation(task: &Task, reason: &str) -> String {
    judge::continuation(&goal(task), reason)
}
