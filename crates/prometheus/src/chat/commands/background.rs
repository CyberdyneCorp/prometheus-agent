//! `/background <prompt>` (alias `/bg`) in the line interface and the TUI
//! (spec: messaging-gateway, Background sessions from chat): the prompt
//! runs as the one turn of a new session of the same profile, on the
//! asking session's model and without its history, while the asking
//! session stays free. The answer is posted as a notification when the
//! task ends. A session runs at most `gateway.background.max_per_chat`
//! tasks at once, as a chat does. The task's session stays (unloaded),
//! so `prometheus chat --resume <id>` can continue it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pmt_agent::turn::{TurnInput, TurnOptions};
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_core::id::{BackgroundId, SessionId};
use pmt_core::tasks;

use super::Ctx;

/// Tasks running per asking session.
fn running() -> &'static Mutex<HashMap<SessionId, u32>> {
    static RUNNING: OnceLock<Mutex<HashMap<SessionId, u32>>> = OnceLock::new();
    RUNNING.get_or_init(Mutex::default)
}

/// Counts a task of `session` for as long as it lives.
struct Slot(SessionId);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut running = running().lock().expect("background poisoned");
        if let Some(n) = running.get_mut(&self.0) {
            *n = n.saturating_sub(1);
        }
    }
}

/// Takes a slot of `session`, unless `max` tasks run already.
fn slot(session: SessionId, max: u32) -> Option<Slot> {
    let mut running = running().lock().expect("background poisoned");
    let n = running.entry(session).or_default();
    if *n >= max {
        return None;
    }
    *n += 1;
    Some(Slot(session))
}

/// Starts the task; answers at once.
pub(super) async fn background(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let prompt = args.trim();
    if prompt.is_empty() {
        return Err(Error::new(&codes::CLI_USAGE, "usage: /background <prompt>"));
    }
    let max = ctx.scope.config().gateway.background.max_per_chat;
    let Some(slot) = slot(ctx.live.id, max) else {
        return Ok(tasks::too_many(max));
    };
    let factory = ctx.factory;
    let target = ctx.live.model.target();
    let id = factory.create_session(None, &target)?;
    let live = factory.open(id, &target).await?;
    let task = BackgroundId::new().to_string();
    let post = Arc::clone(&ctx.post);
    let text = prompt.to_owned();
    tracing::info!(task, session = %id, "background task started");
    tokio::spawn(async move {
        let _slot = slot;
        let cancel = CancelToken::new();
        let options = TurnOptions::new(&cancel);
        let result = live
            .agent
            .run_turn(Some(&live.scope), live.id, TurnInput::text(text), options)
            .await;
        let notice = match result {
            Ok(done) if done.exit_reason.as_str() == "completed" => {
                tasks::complete(done.final_response.as_deref().unwrap_or_default())
            }
            Ok(done) => tasks::failed(&done.exit_reason.as_str().replace('_', " ")),
            Err(err) => tasks::failed(err.message()),
        };
        // Left open (unloaded), so it can be resumed like any session.
        live.unload().await;
        post(notice);
    });
    Ok(tasks::started(prompt, &task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_gets_at_most_max_slots_and_frees_them() {
        let session = SessionId::new();
        let first = slot(session, 2).unwrap();
        let _second = slot(session, 2).unwrap();
        assert!(slot(session, 2).is_none());
        drop(first);
        assert!(slot(session, 2).is_some());
    }
}
