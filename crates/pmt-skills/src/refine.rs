//! `/refine [focus]` (spec: skills, /refine runs self-improvement review
//! on demand).
//!
//! The same background review the creation nudge starts, run now, with
//! the user's focus added to its instruction. It works in its own hidden
//! child session over the stored conversation, so the live session's
//! prompt, history and cache are untouched; the summary of what it
//! created or patched is posted back to the chat.

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_agent::turn::TurnResult;
use pmt_core::error::{Error, codes};

use crate::reload::{self, Changes};
use crate::review::{self, ReviewRun};
use crate::session::ReviewGuard;

/// What a finished refine reports.
#[derive(Debug)]
pub struct Refined {
    /// The chat message.
    pub summary: String,
    /// The skill changes the review made.
    pub changes: Changes,
    /// The review turn.
    pub result: TurnResult,
}

/// Runs the review now and summarises its skill changes.
///
/// # Errors
///
/// `skill.review_running` (reply [`review::ALREADY_RUNNING`]) while a
/// review of the session runs; store or turn errors.
pub async fn refine(run: ReviewRun) -> Result<Refined, Error> {
    let running = run
        .parent
        .begin_review()
        .ok_or_else(|| Error::new(&codes::SKILL_REVIEW_RUNNING, review::ALREADY_RUNNING))?;
    refine_held(run, running).await
}

/// [`refine`] under a review slot the caller already holds
/// ([`SkillsSession::begin_review`](crate::session::SkillsSession::begin_review)).
///
/// # Errors
///
/// Store or turn errors.
pub async fn refine_held(run: ReviewRun, running: ReviewGuard) -> Result<Refined, Error> {
    let parent = Arc::clone(&run.parent);
    let before = reload::scan(&parent);
    let result = review::run_held(run, running).await?;
    let changes = Changes::between(&before, &reload::scan(&parent));
    Ok(Refined {
        summary: summary(&changes, &result),
        changes,
        result,
    })
}

/// The chat message of a finished review.
#[must_use]
pub fn summary(changes: &Changes, result: &TurnResult) -> String {
    let mut text = String::from("Self-improvement review finished.");
    let skills = &changes.skills;
    for (label, names) in [
        ("Created skills", &skills.added),
        ("Patched skills", &skills.changed),
        ("Removed skills", &skills.removed),
    ] {
        if !names.is_empty() {
            let _ = write!(text, "\n{label}: {}", names.join(", "));
        }
    }
    if changes.is_empty() {
        text.push_str("\nNo skill changed.");
    }
    if let Some(reply) = result
        .final_response
        .as_deref()
        .filter(|r| !r.trim().is_empty())
    {
        let _ = write!(text, "\n{}", reply.trim());
    }
    text
}
