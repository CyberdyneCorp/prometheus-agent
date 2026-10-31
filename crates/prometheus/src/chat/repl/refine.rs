//! `/refine [focus]` (spec: skills, /refine runs self-improvement review
//! on demand). The memory and skill reviews start now, each in its own
//! hidden child session over the stored conversation, so the live
//! session's prompt, history and cache stay untouched; the summary is
//! posted to the chat when both finish.

use std::sync::{Arc, PoisonError};

use pmt_agent::review::child_session;
use pmt_agent::turn::{ToolPort as _, TurnResult};
use pmt_core::error::Error;
use pmt_skills::review::{ALREADY_RUNNING, ReviewRun};
use pmt_skills::session::SkillSessions;

use super::Repl;
use crate::chat::session::Live;

impl Repl {
    /// Starts the reviews in the background; replies at once.
    pub(super) async fn refine(&self, live: &Live, args: &str) -> Result<String, Error> {
        // Taken here so a second `/refine` is refused before anything
        // starts; the skill review holds it until it ends.
        let Some(running) = live.skills.begin_review() else {
            return Ok(ALREADY_RUNNING.to_owned());
        };
        let focus = Some(args.trim().to_owned()).filter(|f| !f.is_empty());
        let skills = self.skill_review(live, focus.clone()).await?;
        let memory = pmt_memory::review::ReviewRun {
            parent: Arc::clone(&live.memory),
            session: live.id,
            child: child_session(&self.factory.store, live.id)?,
            model: Arc::clone(&live.model) as _,
            focus,
        };
        let posted = self.posted.clone();
        tokio::spawn(async move {
            let (skills, memory) = tokio::join!(
                pmt_skills::refine::refine_held(skills, running),
                pmt_memory::review::refine(memory),
            );
            let _ = posted.send(summary(skills, memory));
        });
        Ok(
            "Self-improvement review started; its summary is posted here when it finishes."
                .to_owned(),
        )
    }

    /// A background skill review of `live` in a new hidden child session,
    /// with the skills toolset bound to that child.
    async fn skill_review(&self, live: &Live, focus: Option<String>) -> Result<ReviewRun, Error> {
        let child = child_session(&self.factory.store, live.id)?;
        Ok(ReviewRun {
            parent: Arc::clone(&live.skills),
            session: live.id,
            child,
            model: Arc::clone(&live.model) as _,
            tools: self.factory.review_tools(live, child).await? as _,
            sessions: SkillSessions::process(),
            focus,
        })
    }

    /// Feeds a finished turn to the skill nudge and, when a review is due,
    /// runs it after the reply (spec: skills, Autonomous skill creation
    /// and improvement); a review already running skips this one.
    pub(super) async fn skill_nudge(&self, live: &Live, result: &TurnResult) {
        let available = live
            .tools
            .definitions()
            .iter()
            .any(|t| t.name == "skill_manage");
        let due = live
            .skill_nudge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(result, available);
        if !due {
            return;
        }
        let review = match self.skill_review(live, None).await {
            Ok(review) => review,
            Err(err) => {
                tracing::warn!(error = %err, "skill review skipped");
                return;
            }
        };
        tokio::spawn(async move {
            if let Err(err) = pmt_skills::review::run(review).await {
                tracing::warn!(error = %err, "skill review failed");
            }
        });
    }
}

/// The chat message of the finished reviews.
fn summary(
    skills: Result<pmt_skills::refine::Refined, Error>,
    memory: Result<(pmt_memory::review::Changes, TurnResult), Error>,
) -> String {
    let mut text = match skills {
        Ok(refined) => refined.summary,
        Err(err) => format!(
            "Self-improvement review: the skill review failed: {}",
            err.message()
        ),
    };
    let memory = match memory {
        Ok((changes, _)) => changes.summary(),
        Err(err) => format!("Memory: the review failed: {}", err.message()),
    };
    text.push('\n');
    text.push_str(&memory);
    text
}
