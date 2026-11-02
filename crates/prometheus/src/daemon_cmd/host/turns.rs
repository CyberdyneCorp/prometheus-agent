//! Admitting inputs to a daemon session (spec: tui, Prompt submission
//! while busy; Turn authors are stamped by the gateway; Orphaned
//! attachments are reaped). An input arriving while a turn runs is never
//! dropped: `busy.input_mode` queues it (first in, first out), steers the
//! running turn with it after its current tool step, or interrupts the
//! turn at its next safe point and runs it next. Each admitted input
//! carries its gateway-stamped author into its stored user message.

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

use super::{DaemonHost, Loaded, Profile, finished, session_id, setup};

/// An input waiting for its turn.
pub(super) struct Queued {
    input: TurnInput,
    turn_id: String,
    run_budget: Option<u64>,
    link: Link,
}

impl DaemonHost {
    pub(super) async fn submit_turn(
        &self,
        profile: &Profile,
        params: SubmitParams,
        author: Author,
        link: Link,
    ) -> Result<Submitted, Error> {
        let id = session_id(&params.session_id)?;
        Self::check_budget(profile, id)?;
        let session = self.load(profile, id, &link).await?;
        let mut input = setup::input(&session.factory, &session.live, &params).await?;
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
                self.start(id, Arc::clone(&session), job)?;
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
    /// own; the session is idle again once the queue is empty.
    fn start(&self, id: SessionId, session: Arc<Loaded>, first: Queued) -> Result<(), Error> {
        let turns = Arc::clone(&self.turns);
        let mut record = match setup::record(&session.factory, &session.live) {
            Ok(record) => record,
            Err(err) => {
                session.busy.store(false, Ordering::Release);
                return Err(err);
            }
        };
        turns.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            let mut job = first;
            loop {
                run_one(id, &session, job, &record).await;
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
            turns.fetch_sub(1, Ordering::AcqRel);
        });
        Ok(())
    }

    /// Closes a loaded session no client uses, unless it runs a turn, has
    /// queued input or is in use by a request right now; `true` when it is
    /// closed (or was not loaded).
    pub(super) async fn close_unused(profile: &Profile, id: SessionId) -> bool {
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
                session
                    .live
                    .close(&profile.store, EndReason::IdleTimeout)
                    .await;
                true
            }
            Err(shared) => {
                loaded.insert(id, shared);
                false
            }
        }
    }
}

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
