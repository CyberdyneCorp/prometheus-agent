//! The background reviews a finished turn may trigger, the same on every
//! surface that runs turns (the line interface and daemon sessions: TUI,
//! attached chats, messaging conversations): the memory review the nudge
//! is due for (spec: memory, Memory nudge interval; Background review
//! controls) and the skill review (spec: skills, Autonomous skill creation
//! and improvement). Both run after the reply, on a task of their own.

use std::sync::{Arc, PoisonError};

use pmt_agent::turn::{ToolPort as _, TurnResult};

use crate::chat::session::{Factory, Live};

/// Feeds a finished turn to the memory and skill nudges and spawns the
/// reviews that are due.
pub async fn after_turn(factory: &Factory, live: &Live, result: &TurnResult) {
    if live.memory.end_turn() {
        spawn_memory_review(factory, live).await;
    }
    skill_nudge(factory, live, result).await;
}

/// Runs the background memory review the session is due for: not with
/// `memory.review.enabled = false`, one per session at a time, after the
/// machine is idle when the review model is local and `defer = auto`.
async fn spawn_memory_review(factory: &Factory, live: &Live) {
    let controls = factory.scope.config().memory.review.clone();
    if !controls.enabled {
        return;
    }
    let Some(slot) = pmt_memory::review_controls::Slot::take(&live.id.to_string()) else {
        return;
    };
    let store = Arc::clone(&factory.store);
    let child = match pmt_agent::review::child_session(&store, live.id) {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(error = %err, "memory review skipped");
            return;
        }
    };
    let (model, digest) = factory.review_model(&live.model).await;
    let review = pmt_memory::review::ReviewRun {
        parent: Arc::clone(&live.memory),
        session: live.id,
        child,
        model,
        focus: None,
        suggestions: Some(crate::cron_host::inbox(
            &factory.scope,
            Arc::clone(&factory.store),
        )),
        parent_tools: Some(Arc::clone(&live.tools) as _),
        digest,
    };
    tokio::spawn(async move {
        let _slot = slot;
        let route = review.model.route().await;
        pmt_memory::review_controls::wait_for_turn(&controls, &route).await;
        if let Err(err) = pmt_memory::review::run(review).await {
            tracing::warn!(error = %err, "memory review failed");
        }
    });
}

/// Feeds a finished turn to the skill nudge and, when a review is due,
/// runs it after the reply; a review already running skips this one.
async fn skill_nudge(factory: &Factory, live: &Live, result: &TurnResult) {
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
    let review = match crate::chat::commands::skill_review(factory, live, None).await {
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
