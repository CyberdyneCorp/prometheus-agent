//! The respawn guard (spec: kanban, Respawn guard and dispatch dry run).
//! Before it spawns a worker for a `ready` task the dispatcher asks
//! whether the task should rest: its previous run failed on a quota,
//! authentication or HTTP 429 error (`blocker_auth`), it completed a run
//! within `kanban.respawn_success_window_secs` (`recent_success`), or a
//! comment within `kanban.respawn_pr_window_secs` links a GitHub pull
//! request (`active_pr`). A deliberate re-queue (unblock, requeue,
//! re-promotion) lifts `blocker_auth` and `recent_success`; a later
//! reassign, request-changes or review reopen naming another profile
//! lifts `active_pr`. A guard is never a failure, and the review lane is
//! never guarded.

use pmt_core::error::Error;

use crate::board::Board;
use crate::contract::links_pr;
use crate::model::{Event, Task};

/// Why a ready task is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// The previous run failed on quota, authentication or HTTP 429.
    BlockerAuth,
    /// A run completed recently.
    RecentSuccess,
    /// A recent comment links a pull request.
    ActivePr,
}

impl Guard {
    /// The reason's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockerAuth => "blocker_auth",
            Self::RecentSuccess => "recent_success",
            Self::ActivePr => "active_pr",
        }
    }
}

/// The guard windows (Unix ms).
#[derive(Debug, Clone, Copy)]
pub struct Windows {
    /// `kanban.respawn_success_window_secs`.
    pub success_ms: i64,
    /// `kanban.respawn_pr_window_secs`.
    pub pr_ms: i64,
}

/// Events that deliberately put a task back in the queue.
const REQUEUES: [&str; 4] = ["unblocked", "promoted", "restored", "moved"];
/// Events that hand a task to (possibly) another profile.
const HANDOVERS: [&str; 3] = ["assigned", "changes_requested", "review_reopened"];

fn auth_failure(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "quota",
        "authentication",
        "unauthorized",
        "401",
        "403",
        "429",
        "rate limit",
        "invalid api key",
        "insufficient_quota",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn requeued_after(events: &[Event], at: i64) -> bool {
    events
        .iter()
        .any(|e| e.at >= at && REQUEUES.contains(&e.kind.as_str()))
}

/// The profile an event's actor label (`kind:name`) names.
fn actor_profile(label: &str) -> &str {
    label.split_once(':').map_or(label, |(_, name)| name)
}

/// Why `task` should not be spawned now, if it should not.
///
/// # Errors
///
/// Store errors.
pub fn check(
    board: &Board,
    task: &Task,
    windows: Windows,
    now: i64,
) -> Result<Option<Guard>, Error> {
    let runs = board.runs(&task.id)?;
    let events = board.events(Some(&task.id))?;
    let last = runs
        .iter()
        .rev()
        .find(|r| r.role == "worker" && !r.is_open());
    if let Some(run) = last {
        let ended = run.ended_at.unwrap_or(run.started_at);
        let failed_on_auth = run.outcome.as_deref() != Some("rate_limited")
            && run.outcome.as_deref() != Some("completed")
            && run.error.as_deref().is_some_and(auth_failure);
        if failed_on_auth && !requeued_after(&events, ended) {
            return Ok(Some(Guard::BlockerAuth));
        }
    }
    // A run that finished its work: completed, or handed to review.
    let success = runs
        .iter()
        .rev()
        .find(|r| matches!(r.outcome.as_deref(), Some("completed" | "review")))
        .and_then(|r| r.ended_at);
    if let Some(at) = success
        && at + windows.success_ms > now
        && !requeued_after(&events, at)
    {
        return Ok(Some(Guard::RecentSuccess));
    }
    let comments = board.comments(&task.id)?;
    let pr = comments
        .iter()
        .rev()
        .find(|c| c.at + windows.pr_ms > now && links_pr(&c.body));
    if let Some(comment) = pr {
        let author = actor_profile(&comment.author);
        let handed_over = events
            .iter()
            .any(|e| e.at >= comment.at && HANDOVERS.contains(&e.kind.as_str()))
            && task.assignee.as_deref().is_some_and(|a| a != author);
        if !handed_over {
            return Ok(Some(Guard::ActivePr));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_and_auth_failures_are_recognised() {
        assert!(auth_failure("provider said HTTP 429 Too Many Requests"));
        assert!(auth_failure("Invalid API key"));
        assert!(!auth_failure("worker exited with code 1"));
    }
}
