//! Crash resume mid-tool-batch (spec: agent-turn-loop, Crash resume
//! mid-tool-batch).
//!
//! A session whose journal's newest turn has no `turn_finished` was
//! interrupted by a crash. Resuming it continues that turn: when the last
//! stored row is the assistant's unanswered tool calls, every call whose
//! `tool_finished` was journaled reuses its stored result, a call that
//! never started runs, a started-but-unfinished call runs again only when
//! its tool declared `effect = "none"` or `idempotent = true`, and every
//! other started call gets the outcome-unknown result; then the loop goes
//! on with the next model request. Declining (or `agent.resume_on_crash =
//! "never"`, or a new message arriving first) closes the turn as `crashed`
//! with an assistant boundary row.

use std::collections::{HashMap, HashSet};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::TurnStart;
use pmt_store::{BlobRef, JournalKind, JournalRecord, Message, Role, Visibility};
use pmt_tools::ToolResult;

use super::exit::{ExitReason, TurnId, TurnResult};
use super::lease::{self, Refresher};
use super::model::RequestedCall;
use super::run::{Ending, Turn};
use super::tools::ToolOutcome;
use super::{Agent, TurnOptions};

/// A turn a crash left unfinished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    /// The turn's id.
    pub turn_id: String,
    records: Vec<JournalRecord>,
}

impl Unfinished {
    /// The tools that had started and not finished when the process
    /// stopped.
    #[must_use]
    pub fn interrupted_tools(&self) -> Vec<String> {
        let finished: HashSet<_> = self.finished().into_keys().collect();
        self.records
            .iter()
            .filter(|r| r.kind == JournalKind::ToolStarted)
            .filter(|r| r.call_id.as_ref().is_none_or(|c| !finished.contains(c)))
            .filter_map(|r| r.tool.clone())
            .collect()
    }

    /// Call id to stored result ref, for finished calls.
    fn finished(&self) -> HashMap<String, String> {
        self.records
            .iter()
            .filter(|r| r.kind == JournalKind::ToolFinished)
            .filter_map(|r| Some((r.call_id.clone()?, r.data.clone()?)))
            .collect()
    }

    /// Call id to "may run again", for started calls.
    fn started(&self) -> HashMap<String, bool> {
        self.records
            .iter()
            .filter(|r| r.kind == JournalKind::ToolStarted)
            .filter_map(|r| {
                let rerunnable = r.idempotent == Some(true) || r.effect.as_deref() == Some("none");
                Some((r.call_id.clone()?, rerunnable))
            })
            .collect()
    }
}

/// The result a started call gets when it may not run again.
#[must_use]
pub fn outcome_unknown(tool: &str) -> String {
    format!(
        "[prometheus] outcome unknown: the process stopped while '{tool}' was running. Verify its effect before retrying."
    )
}

/// The result a call that never started gets when the turn is closed.
fn not_run(tool: &str) -> String {
    format!("[prometheus] '{tool}' did not run: the process stopped before it started.")
}

/// What the stored transcript of a crashed turn ends with.
enum Tail {
    /// The assistant's tool calls, unanswered.
    Calls(Vec<RequestedCall>),
    /// A final answer (only `turn_finished` was missing).
    Answer(String),
    /// A user or tool row: the next request is due.
    Request(Option<Role>),
}

fn tail(rows: &[Message]) -> Tail {
    let Some(last) = rows.last() else {
        return Tail::Request(None);
    };
    if last.role != Role::Assistant {
        return Tail::Request(Some(last.role));
    }
    let calls: Vec<RequestedCall> = last
        .tool_calls
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .map(|calls| calls.iter().filter_map(requested).collect())
        .unwrap_or_default();
    if calls.is_empty() {
        Tail::Answer(last.content.clone().unwrap_or_default())
    } else {
        Tail::Calls(calls)
    }
}

fn requested(call: &serde_json::Value) -> Option<RequestedCall> {
    Some(RequestedCall {
        id: call.get("id")?.as_str()?.to_owned(),
        name: call.get("name")?.as_str()?.to_owned(),
        arguments: call.get("arguments")?.as_str()?.to_owned(),
    })
}

impl Agent {
    /// This session's turn a crash left unfinished, if any.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn unfinished_turn(&self) -> Result<Option<Unfinished>, Error> {
        let setup = &self.setup;
        Ok(setup
            .store
            .unfinished_turn(setup.session)?
            .and_then(|records| {
                let turn_id = records.first()?.turn_id.clone();
                Some(Unfinished { turn_id, records })
            }))
    }

    /// Continues `unfinished` (`resume = true`) or closes it as `crashed`
    /// with a boundary row, under the session's turn lease.
    ///
    /// # Errors
    ///
    /// `turn.scope_required`, `turn.scope_mismatch`, store errors.
    pub async fn resume_turn(
        &self,
        scope: Option<&ProfileScope>,
        unfinished: Unfinished,
        resume: bool,
        options: TurnOptions,
    ) -> Result<TurnResult, Error> {
        let scope = self.check_scope(scope, self.setup.session)?;
        let setup = &self.setup;
        let lease = lease::admit(&setup.store, setup.session, options.lease).await?;
        let refresher = Refresher::start(
            setup.store.clone(),
            lease.clone(),
            options.lease.renew_every,
            options.interrupt.clone(),
        );
        let mut turn = Turn::new(
            self,
            scope.config(),
            TurnId::recorded(&unfinished.turn_id),
            options,
        );
        let result = if resume {
            turn.resume(&unfinished).await
        } else {
            turn.close_crashed(&unfinished).await
        };
        drop(refresher);
        if let Err(err) = setup.store.release_lease(&lease) {
            tracing::warn!(error = %err, "turn lease could not be released; it lapses at its expiry");
        }
        result
    }
}

impl Turn<'_> {
    /// Closes a crashed turn before a new one starts (the new message
    /// declines the resume).
    pub(super) async fn close_unfinished(&mut self) -> Result<(), Error> {
        let Some(unfinished) = self.agent.unfinished_turn()? else {
            return Ok(());
        };
        let mut old = Turn::new(
            self.agent,
            self.config,
            TurnId::recorded(&unfinished.turn_id),
            super::TurnOptions::default(),
        );
        old.close_crashed(&unfinished).await.map(drop)
    }

    fn stored_rows(&self) -> Result<Vec<Message>, Error> {
        let setup = &self.agent.setup;
        setup.store.messages(setup.session, Visibility::Active)
    }

    /// Continues the turn from its journal.
    async fn resume(mut self, unfinished: &Unfinished) -> Result<TurnResult, Error> {
        let setup = &self.agent.setup;
        setup.model.start_turn().await;
        self.start_stream();
        let route = setup.model.route().await;
        let mut frozen = self.freeze(&route).await?;
        self.emit(Event::TurnStart(TurnStart { message_id: None }))
            .await;
        match tail(&self.stored_rows()?) {
            Tail::Calls(calls) => self.finish_batch(&calls, unfinished).await?,
            Tail::Answer(text) => {
                let mut end = Ending::new(ExitReason::Completed);
                end.final_response = Some(text);
                return self.finish(end, Some(&frozen)).await;
            }
            Tail::Request(role) => self.last_role = role,
        }
        let ending = self.iterate(&mut frozen).await?;
        self.finish(ending, Some(&frozen)).await
    }

    /// Answers the unanswered calls: stored results reused, the safe ones
    /// run, the others marked outcome unknown.
    async fn finish_batch(
        &mut self,
        calls: &[RequestedCall],
        unfinished: &Unfinished,
    ) -> Result<(), Error> {
        let finished = unfinished.finished();
        let started = unfinished.started();
        let mut outcomes: Vec<Option<ToolOutcome>> = Vec::with_capacity(calls.len());
        let mut to_run = Vec::new();
        for call in calls {
            if let Some(result) = finished.get(&call.id).and_then(|r| self.stored_result(r)) {
                outcomes.push(Some(ToolOutcome {
                    result,
                    duration_ms: 0,
                }));
            } else if started.get(&call.id) == Some(&false) {
                outcomes.push(Some(ToolOutcome {
                    result: ToolResult::error(outcome_unknown(&call.name)),
                    duration_ms: 0,
                }));
            } else {
                outcomes.push(None);
                to_run.push(call.clone());
            }
        }
        let mut ran = self.run_again(&to_run).await.into_iter();
        let outcomes: Vec<ToolOutcome> = outcomes
            .into_iter()
            .map(|o| {
                o.or_else(|| ran.next()).unwrap_or_else(|| ToolOutcome {
                    result: ToolResult::error("no result"),
                    duration_ms: 0,
                })
            })
            .collect();
        self.keep_results(calls, &outcomes).await
    }

    /// Runs `calls` as one journaled batch.
    async fn run_again(&self, calls: &[RequestedCall]) -> Vec<ToolOutcome> {
        if calls.is_empty() {
            return Vec::new();
        }
        let token = self.options.interrupt.token().child();
        let tools = self.agent.setup.tools.clone();
        let journal = self.batch_journal(calls);
        let turn_id = self.id.to_string();
        let raced = self
            .race_tools(tools.run_journaled(calls, &turn_id, &token, journal))
            .await;
        raced.map_or_else(
            |_| {
                token.cancel();
                Vec::new()
            },
            |batch| batch.outcomes,
        )
    }

    fn stored_result(&self, blob: &str) -> Option<ToolResult> {
        let blob = BlobRef::parse(blob)?;
        let bytes = self.agent.setup.store.get_blob(&blob).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Closes the turn as `crashed`: unanswered calls get their stored
    /// result or a note, then an assistant boundary row.
    async fn close_crashed(&mut self, unfinished: &Unfinished) -> Result<TurnResult, Error> {
        match tail(&self.stored_rows()?) {
            Tail::Calls(calls) => {
                let finished = unfinished.finished();
                let started = unfinished.started();
                let outcomes: Vec<ToolOutcome> = calls
                    .iter()
                    .map(|call| {
                        let result = finished
                            .get(&call.id)
                            .and_then(|r| self.stored_result(r))
                            .unwrap_or_else(|| {
                                ToolResult::error(if started.contains_key(&call.id) {
                                    outcome_unknown(&call.name)
                                } else {
                                    not_run(&call.name)
                                })
                            });
                        ToolOutcome {
                            result,
                            duration_ms: 0,
                        }
                    })
                    .collect();
                self.keep_results(&calls, &outcomes).await?;
            }
            Tail::Answer(_) => self.last_role = Some(Role::Assistant),
            Tail::Request(role) => self.last_role = role,
        }
        let ending = Ending::failed(
            ExitReason::Crashed,
            Error::new(
                &codes::TURN_CRASHED,
                "the process stopped while this turn ran",
            ),
        );
        self.close_failed(&ending)?;
        self.journal(
            &JournalRecord::new(self.id.as_str(), JournalKind::TurnFinished)
                .with_data(ExitReason::Crashed.as_str()),
        )?;
        self.finish(ending, None).await
    }
}
