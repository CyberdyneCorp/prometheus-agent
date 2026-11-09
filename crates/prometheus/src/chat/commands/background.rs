//! `/background <prompt>` (alias `/bg`) in the line interface, the TUI and
//! daemon client sessions (spec: messaging-gateway, Background sessions
//! from chat; subagent-delegation, User-launched background sessions):
//! the prompt runs as the one turn of a new, independent session of the
//! same profile (`source = background`, its parent the asking session for
//! lineage only, no history of it), on the asking session's route,
//! reasoning level, fallback chain and tool selection,
//! while the asking session stays free. It takes a slot of
//! `delegation.max_concurrent_children` like a child (waiting for one when
//! all are busy). The answer is posted as a labelled notification when the
//! task ends, never inserted into the asking session. `/agents` lists and
//! stops it. A session runs at most `gateway.background.max_per_chat`
//! tasks at once, as a chat does. The task's session stays (unloaded), so
//! `prometheus chat --resume <id>` can continue it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pmt_agent::turn::{Interrupt, Issuer, TurnInput, TurnOptions, TurnResult};
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_core::id::{BackgroundId, SessionId};
use pmt_core::tasks::{self, NewTask, Registry, State, Task};
use pmt_delegation::limiter::{Admission, Limiter};
use pmt_store::{LineageKind, NewSession, Source};

use super::Ctx;
use crate::chat::session::Live;

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
    let delegation = &ctx.scope.config().delegation;
    let admission = Limiter::for_profile(
        ctx.scope.dir(),
        delegation.max_concurrent_children,
        delegation.max_queued,
    )
    .admit(&ctx.live.id.to_string(), 1)
    .map_err(|refusal| refusal.error())?
    .pop()
    .expect("one admission per task");
    let target = ctx.live.model.target();
    let live = open_session(ctx, &target).await?;
    let interrupt = Interrupt::default();
    let stop = interrupt.clone();
    let task = Registry::process().add(NewTask {
        id: BackgroundId::new().to_string(),
        launcher: ctx.live.id.to_string(),
        prompt: prompt.to_owned(),
        model: format!(
            "{}/{}",
            target.provider,
            target.model.as_deref().unwrap_or_default()
        ),
        stop: Box::new(move || stop.stop(Issuer::User)),
    });
    task.set_session(&live.id.to_string());
    tracing::info!(task = task.id, session = %live.id, "background task started");
    let started = tasks::started(task.number, prompt, &task.id);
    let run = Run {
        live,
        task,
        interrupt,
        prompt: prompt.to_owned(),
    };
    let post = Arc::clone(&ctx.post);
    tokio::spawn(async move {
        let _slot = slot;
        let notice = run.run(admission).await;
        post(notice);
    });
    Ok(started)
}

/// The task's own session: independent of the asking one (no history,
/// its own lineage root), recorded with `source = background` and the
/// asking session as its parent for lineage only, on the same route,
/// reasoning level, working directory and tool selection (including
/// `/tools ... --now` changes of the asking session).
async fn open_session(
    ctx: &Ctx<'_>,
    target: &pmt_providers::resolve::Target,
) -> Result<Live, Error> {
    let mut new = NewSession::child(Source::Background, ctx.live.id, LineageKind::Background);
    new.provider = Some(target.provider.clone());
    new.model.clone_from(&target.model);
    new.cwd = Some(ctx.live.cwd().to_string_lossy().into_owned());
    let id = ctx.factory.store.create_session(&new)?.id;
    let live = ctx.factory.open(id, target).await?;
    live.model
        .set_reasoning(ctx.live.model.session().reasoning());
    ctx.factory
        .rebuild_tools(&live, &ctx.live.selection())
        .await?;
    Ok(live)
}

/// A started task.
struct Run {
    live: Live,
    task: Arc<Task>,
    interrupt: Interrupt,
    prompt: String,
}

impl Run {
    /// Waits for a slot, runs the turn and ends the task; the notice to
    /// post.
    async fn run(self, admission: Admission) -> String {
        let Self {
            live,
            task,
            interrupt,
            prompt,
        } = self;
        let permit = tokio::select! {
            permit = admission.ready() => permit,
            () = interrupt.token().cancelled() => None,
        };
        let (state, notice) = match permit {
            Some(_permit) => {
                task.running();
                let options = TurnOptions {
                    interrupt: interrupt.clone(),
                    ..TurnOptions::new(&CancelToken::new())
                };
                let result = live
                    .agent
                    .run_turn(Some(&live.scope), live.id, TurnInput::text(prompt), options)
                    .await;
                outcome(task.number, result)
            }
            None => (State::Interrupted, tasks::failed(task.number, "stopped")),
        };
        tracing::info!(
            task = task.id,
            state = state.name(),
            "background task finished"
        );
        task.finish(state);
        // Left open (unloaded), so it can be resumed like any session.
        live.unload().await;
        notice
    }
}

/// The task's end state and notice from its turn.
fn outcome(number: u32, result: Result<TurnResult, Error>) -> (State, String) {
    match result {
        Ok(done) if done.exit_reason.as_str() == "completed" => (
            State::Completed,
            tasks::complete(number, done.final_response.as_deref().unwrap_or_default()),
        ),
        Ok(done) if done.exit_reason.as_str().starts_with("interrupted") => {
            (State::Interrupted, tasks::failed(number, "stopped"))
        }
        Ok(done) => {
            let reason = done.exit_reason.as_str().replace('_', " ");
            (
                State::Failed(reason.clone()),
                tasks::failed(number, &reason),
            )
        }
        Err(err) => (
            State::Failed(err.message().to_owned()),
            tasks::failed(number, err.message()),
        ),
    }
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
