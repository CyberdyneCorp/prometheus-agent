//! Admitting inputs to a daemon session (spec: tui, Prompt submission
//! while busy; Turn authors are stamped by the gateway; Orphaned
//! attachments are reaped). An input arriving while a turn runs is never
//! dropped: `busy.input_mode` queues it (first in, first out), steers the
//! running turn with it after its current tool step, or interrupts the
//! turn at its next safe point and runs it next. Each admitted input
//! carries its gateway-stamped author into its stored user message.
//!
//! Every loaded session is ended when it leaves the daemon (spec:
//! session-storage, ended sessions): an idle one by the reaper
//! (`idle_timeout`), a one-shot run's once its turn is done and every
//! other one when the daemon stops or restarts (`user_exit`, or
//! `interrupted` for a turn still running after the drain), so the store
//! and the trajectory say the same as for an in-process run.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pmt_agent::turn::{TurnInput, TurnOptions};
use pmt_config::schema::BusyInputMode;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_gateway::{Author, Link};
use pmt_rpc::contracts::{Admission, SubmitParams, Submitted, TurnEnded};
use pmt_store::EndReason;

use tracing::Instrument as _;

use super::{DaemonHost, Loaded, Profile, finished, session_id, setup};

/// An input waiting for its turn.
pub(super) struct Queued {
    pub(super) input: TurnInput,
    pub(super) turn_id: String,
    pub(super) run_budget: Option<u64>,
    pub(super) link: Link,
}

impl DaemonHost {
    pub(super) async fn submit_turn(
        &self,
        profile: &Arc<Profile>,
        params: SubmitParams,
        author: Author,
        link: Link,
    ) -> Result<Submitted, Error> {
        let id = session_id(&params.session_id)?;
        // No new turn starts while the home is paused (spec: messaging-
        // gateway, Global emergency pause).
        pmt_core::pause::check(profile.scope().home())?;
        Self::check_budget(profile, id)?;
        let session = self.load(profile, id, &link).await?;
        let mut input = setup::input(&session.factory, &session.live, &params).await?;
        input.prefix.clone_from(&author.prefix);
        input.author = serde_json::to_value(&author).ok();
        let job = Queued {
            input,
            turn_id: pmt_agent::turn::TurnId::new().to_string(),
            run_budget: params.run_budget_secs,
            link,
        };
        let turn_id = job.turn_id.clone();
        let admission = {
            let mut queue = session.queue.lock().expect("queue poisoned");
            if session.busy.swap(true, Ordering::AcqRel) {
                Self::while_busy(&session, &mut queue, job, &params.text)?
            } else {
                drop(queue);
                self.start(Arc::clone(profile), id, Arc::clone(&session), job)?;
                return Ok(Submitted {
                    turn_id,
                    admission: Admission::Started,
                });
            }
        };
        Ok(admission)
    }

    /// An input that arrived while a turn runs, by `busy.input_mode`.
    fn while_busy(
        session: &Loaded,
        queue: &mut std::collections::VecDeque<Queued>,
        job: Queued,
        text: &str,
    ) -> Result<Submitted, Error> {
        let mode = session.live.config.get().config().busy.input_mode;
        let turn_id = job.turn_id.clone();
        match mode {
            BusyInputMode::Queue => queue.push_back(job),
            BusyInputMode::Interrupt => {
                queue.push_front(job);
                session.stop();
            }
            BusyInputMode::Steer => {
                session.live.agent.steer(text)?;
                let running = session.current.lock().expect("current poisoned").clone();
                return Ok(Submitted {
                    turn_id: running.unwrap_or(turn_id),
                    admission: Admission::Steered,
                });
            }
        }
        Ok(Submitted {
            turn_id,
            admission: Admission::Queued,
        })
    }

    /// Runs `job`, then every input queued behind it, on a task of its
    /// own; the session is idle again once the queue is empty (and a
    /// one-shot session ended).
    fn start(
        &self,
        profile: Arc<Profile>,
        id: SessionId,
        session: Arc<Loaded>,
        first: Queued,
    ) -> Result<(), Error> {
        Self::start_with(
            Arc::clone(&self.turns),
            self.slots.clone(),
            profile,
            id,
            session,
            first,
        )
    }

    /// [`Self::start`] with the daemon's turn counter and slots passed in
    /// (a delegation completion starts its turn from outside a request).
    pub(super) fn start_with(
        turns: Arc<std::sync::atomic::AtomicUsize>,
        slots: pmt_gateway::slots::Slots,
        profile: Arc<Profile>,
        id: SessionId,
        session: Arc<Loaded>,
        first: Queued,
    ) -> Result<(), Error> {
        let mut record = match setup::record(&session.factory, &session.live) {
            Ok(record) => record,
            Err(err) => {
                session.busy.store(false, Ordering::Release);
                return Err(err);
            }
        };
        turns.fetch_add(1, Ordering::AcqRel);
        // In the submitting request's session span: the turn's records go
        // to the profile's logs.
        tokio::spawn(
            async move {
                let mut job = first;
                loop {
                    let link = job.link.clone();
                    let permit = slots.acquire(|ahead| waiting(&link, ahead)).await;
                    run_one(id, &session, job, &record).await;
                    drop(permit);
                    let next = {
                        let mut queue = session.queue.lock().expect("queue poisoned");
                        let next = queue.pop_front();
                        if next.is_none() {
                            session.busy.store(false, Ordering::Release);
                        }
                        next
                    };
                    let Some(queued) = next else { break };
                    job = queued;
                    record = setup::record(&session.factory, &session.live).unwrap_or_else(|err| {
                        tracing::warn!("no usage record for a queued turn: {err}");
                        crate::chat::usage_file::RunRecord::default()
                    });
                }
                session.idle.notify_waiters();
                let oneshot = session.oneshot;
                drop(session);
                if oneshot {
                    Self::close_loaded(&profile, id, EndReason::UserExit).await;
                }
                turns.fetch_sub(1, Ordering::AcqRel);
            }
            .in_current_span(),
        );
        Ok(())
    }

    /// Closes a loaded session no client uses, unless it runs a turn, has
    /// queued input or is in use by a request right now; `true` when it is
    /// closed (or was not loaded).
    pub(super) async fn close_unused(profile: &Profile, id: SessionId) -> bool {
        {
            let loaded = profile.loaded.lock().await;
            let Some(session) = loaded.get(&id) else {
                return true;
            };
            let queued = !session.queue.lock().expect("queue poisoned").is_empty();
            if session.busy.load(Ordering::Acquire) || queued {
                return false;
            }
        }
        Self::close_loaded(profile, id, EndReason::IdleTimeout).await
    }

    /// Unloads session `id` and ends it with `reason`, unless something
    /// else (a request being answered) holds it right now; `true` when it
    /// is closed (or was not loaded).
    pub(super) async fn close_loaded(profile: &Profile, id: SessionId, reason: EndReason) -> bool {
        let mut loaded = profile.loaded.lock().await;
        let Some(session) = loaded.remove(&id) else {
            return true;
        };
        match Arc::try_unwrap(session) {
            Ok(session) => {
                drop(loaded);
                session.live.close(&profile.store, reason).await;
                true
            }
            Err(shared) => {
                loaded.insert(id, shared);
                false
            }
        }
    }

    /// Unloads an idle session without ending it (the messaging gateway's
    /// agent cache): `false` while it runs a turn, holds queued input or
    /// is in use by a request.
    pub(super) async fn unload(profile: &Profile, id: SessionId) -> bool {
        let mut loaded = profile.loaded.lock().await;
        let Some(session) = loaded.get(&id) else {
            return true;
        };
        let queued = !session.queue.lock().expect("queue poisoned").is_empty();
        if session.busy.load(Ordering::Acquire) || queued {
            return false;
        }
        let Some(session) = loaded.remove(&id) else {
            return true;
        };
        match Arc::try_unwrap(session) {
            Ok(session) => {
                drop(loaded);
                session.live.unload().await;
                true
            }
            Err(shared) => {
                loaded.insert(id, shared);
                false
            }
        }
    }

    /// Ends every session `profile` has loaded (the daemon is stopping;
    /// the turns had their drain time). A turn still running is stopped
    /// and its session ends `interrupted`; a session something still
    /// holds is ended in the store without closing its trajectory file.
    pub(super) async fn close_all(name: &str, profile: &Profile) {
        // Stopping the daemon is not the sessions' end for their background
        // subagents: their completions stay owed and the next start
        // delivers them (spec: subagent-delegation, Durable background
        // completions).
        for session in profile.loaded.lock().await.values() {
            if let Some(delegation) = &session.live.delegation {
                delegation.suspend();
            }
        }
        let ids: Vec<SessionId> = profile.loaded.lock().await.keys().copied().collect();
        for id in ids {
            Self::close_at_stop(profile, id)
                .instrument(super::session_span(name, &id.to_string()))
                .await;
        }
    }

    /// Ends one session at stop: a running turn is stopped first.
    async fn close_at_stop(profile: &Profile, id: SessionId) {
        let Some(session) = profile.loaded.lock().await.get(&id).cloned() else {
            return;
        };
        let reason = if session.busy.load(Ordering::Acquire) {
            session.stop();
            let _ = tokio::time::timeout(STOP_WAIT, session.until_idle()).await;
            EndReason::Interrupted
        } else {
            EndReason::UserExit
        };
        drop(session);
        if !Self::close_loaded(profile, id, reason).await
            && let Some(session) = profile.loaded.lock().await.remove(&id)
        {
            session.live.end(&profile.store, reason).await;
        }
    }
}

/// Tells the session's clients that its turn waits for a free slot: the
/// messaging gateway says it in the chat (`turn.waiting`), terminals show
/// a notice.
fn waiting(link: &Link, ahead: usize) {
    let text = pmt_gateway::slots::waiting_notice(ahead);
    tracing::info!(ahead, "turn waiting for a free slot");
    for (kind, payload) in [
        (
            "turn.waiting",
            serde_json::json!({"ahead": ahead, "text": text}),
        ),
        ("command.notice", serde_json::json!({"text": text})),
    ] {
        link.emit::<pmt_rpc::contracts::SessionEvent>(&serde_json::json!({
            "type": kind,
            "ts": jiff::Timestamp::now().to_string(),
            "turn_id": null,
            "span_id": null,
            "parent_span_id": null,
            "payload": payload,
        }));
    }
}

/// How long a stopping daemon waits for an interrupted turn to end.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Runs one turn and emits its `turn.finished`.
async fn run_one(
    id: SessionId,
    session: &Loaded,
    job: Queued,
    record: &crate::chat::usage_file::RunRecord,
) {
    *session.current.lock().expect("current poisoned") = Some(job.turn_id.clone());
    // The session's configuration: its start snapshot with the live keys
    // of every reload since.
    let scope = session.live.config.get();
    let mut options = TurnOptions::new(&CancelToken::new());
    options.run_budget = job.run_budget.map(Duration::from_secs);
    if job.link.streams() {
        options.deltas = Arc::new(crate::daemon_cmd::turn::DeltasToLink(job.link.clone()));
    }
    *session.interrupt.lock().expect("interrupt poisoned") = Some(options.interrupt.clone());
    let outcome = session
        .live
        .agent
        .run_turn(Some(&scope), id, job.input, options)
        .await;
    let mut ended = finished(job.turn_id, outcome);
    ended.report = serde_json::to_value(record.report(&Ok(0))).ok();
    job.link.emit::<TurnEnded>(&ended);
    session.interrupt.lock().expect("interrupt poisoned").take();
    session.current.lock().expect("current poisoned").take();
}
