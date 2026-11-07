//! `/refine [focus]` (spec: skills, /refine runs self-improvement review
//! on demand). The memory and skill reviews start now, each in its own
//! hidden child session over the stored conversation, so the live
//! session's prompt, history and cache stay untouched; the summary is
//! posted to the chat when both finish.

use std::sync::Arc;

use pmt_agent::review::child_session;
use pmt_agent::turn::TurnResult;
use pmt_core::error::Error;
use pmt_skills::review::{ALREADY_RUNNING, ReviewRun};
use pmt_skills::session::SkillSessions;

use super::Ctx;
use crate::chat::session::{Factory, Live};

/// Starts the reviews in the background; replies at once.
pub(super) async fn refine(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let live = ctx.live;
    // Taken here so a second `/refine` is refused before anything starts;
    // the skill review holds it until it ends.
    let Some(running) = live.skills.begin_review() else {
        return Ok(ALREADY_RUNNING.to_owned());
    };
    let focus = Some(args.trim().to_owned()).filter(|f| !f.is_empty());
    let skills = skill_review(ctx.factory, live, focus.clone()).await?;
    let memory = pmt_memory::review::ReviewRun {
        parent: Arc::clone(&live.memory),
        session: live.id,
        child: child_session(&ctx.factory.store, live.id)?,
        model: Arc::clone(&live.model) as _,
        focus,
        suggestions: Some(crate::cron_host::inbox(
            ctx.scope,
            Arc::clone(&ctx.factory.store),
        )),
    };
    let post = Arc::clone(&ctx.post);
    tokio::spawn(async move {
        let (skills, memory) = tokio::join!(
            pmt_skills::refine::refine_held(skills, running),
            pmt_memory::review::refine(memory),
        );
        post(summary(skills, memory));
    });
    Ok("Self-improvement review started; its summary is posted here when it finishes.".to_owned())
}

/// A background skill review of `live` in a new hidden child session,
/// with the skills toolset bound to that child.
///
/// # Errors
///
/// Store errors creating the child; the skills toolset is missing.
pub(crate) async fn skill_review(
    factory: &Factory,
    live: &Live,
    focus: Option<String>,
) -> Result<ReviewRun, Error> {
    let child = child_session(&factory.store, live.id)?;
    Ok(ReviewRun {
        parent: Arc::clone(&live.skills),
        session: live.id,
        child,
        model: Arc::clone(&live.model) as _,
        tools: factory.review_tools(live, child).await? as _,
        sessions: SkillSessions::process(),
        focus,
    })
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
