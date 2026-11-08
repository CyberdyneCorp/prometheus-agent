//! One compaction attempt, end to end (spec: context-compression, One
//! compaction at a time per session; Compaction is always in place;
//! Summary failure handling; Iterative summary update).
//!
//! The attempt holds the session's compaction lock in `state.db` for its
//! whole length (a second attempt gets `lock_skipped` and changes
//! nothing), reads the active transcript, splits it, prunes the middle,
//! asks for the summary (the previous summary passed as the document to
//! update), assembles the compacted transcript and commits it in place:
//! the replaced rows stay readable as archived rows of the new epoch. The
//! guards in [`pmt_store::CompressionState`] are updated on every outcome.
//! Deciding WHETHER to compact is the caller's ([`super::decision`]).

use std::time::{Duration, Instant};

use jiff::Timestamp;
use pmt_config::schema::CompressionConfig;
use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{CompressionCommitted, CompressionFailed, CompressionStarted};
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{
    Acquire, Compacted, Compaction, Holder, Lease, LeaseKind, Message as Row, Store, Visibility,
};
use pmt_tools::environments::Environments;

use super::assemble::{Assembled, Item, assemble};
use super::engine::ContextEngine;
use super::items::{Items, previous_summary};
use super::pins;
use super::prepared::{Prepared, PreparedSummary};
use super::prune::{Pruned, prune};
use super::quality::{self, Checklist};
use super::split::{Options, Split, split};
use super::summary::{self, FailureKind, Input, Sections, Summarizer, SummaryFailure};
use super::threshold::{ModelWindow, summary_budget, tail_budget};
use super::validate::validate;
use super::{Trigger, decision, is_summary, rows_tokens, tokens};
use crate::tools::todo::TodoList;
use crate::turn::events::EventSink;

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `--preview`: what would happen; nothing changed.
    Preview,
    /// Nothing to summarize.
    NothingToDo,
    /// Another compaction of the session holds the lock.
    LockSkipped,
    /// Committed.
    Compressed,
    /// The summary failed and nothing was committed (auth or quota
    /// errors, `abort_on_summary_failure`, a concurrent rewrite).
    Failed,
}

impl Status {
    /// The status as `/compress` reports it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::NothingToDo => "nothing_to_do",
            Self::LockSkipped => "lock_skipped",
            Self::Compressed => "compressed",
            Self::Failed => "failed",
        }
    }
}

/// The result of an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// How it ended.
    pub status: Status,
    /// The committed epoch.
    pub epoch: Option<u32>,
    /// Context tokens before.
    pub tokens_before: u64,
    /// Context tokens after (estimated for a preview).
    pub tokens_after: u64,
    /// The rows summarized (or that would be).
    pub summarized: Vec<MessageId>,
    /// The summary failure, when there was one.
    pub failure: Option<SummaryFailure>,
    /// The deterministic fallback summary was committed.
    pub fallback: bool,
}

impl Outcome {
    fn of(status: Status, tokens_before: u64) -> Self {
        Self {
            status,
            epoch: None,
            tokens_before,
            tokens_after: tokens_before,
            summarized: Vec::new(),
            failure: None,
            fallback: false,
        }
    }
}

/// What one attempt is asked to do.
#[derive(Debug, Clone)]
pub struct Run {
    /// Why.
    pub trigger: Trigger,
    /// The main model's window.
    pub window: ModelWindow,
    /// The trigger in tokens (sizes a `ratio` tail).
    pub trigger_tokens: u64,
    /// The context size the decision saw (system prompt and tools
    /// included); `None` estimates the transcript.
    pub context_tokens: Option<u64>,
    /// Focus text for the summarizer.
    pub focus: Option<String>,
    /// `here N`: keep exactly the last N user exchanges.
    pub keep_exchanges: Option<u32>,
    /// Report only.
    pub preview: bool,
}

impl Run {
    /// An automatic attempt of `trigger`.
    #[must_use]
    pub fn new(trigger: Trigger, window: ModelWindow, trigger_tokens: u64) -> Self {
        Self {
            trigger,
            window,
            trigger_tokens,
            context_tokens: None,
            focus: None,
            keep_exchanges: None,
            preview: false,
        }
    }
}

/// Compacts one session.
pub struct Compactor<'a> {
    /// The profile's store.
    pub store: &'a Store,
    /// The session.
    pub session: SessionId,
    /// `[compression]`.
    pub config: &'a CompressionConfig,
    /// The summary writer; `None` commits the fallback summary.
    pub summarizer: Option<&'a dyn Summarizer>,
    /// A plugin's context engine (`context.engine`); `None` is the
    /// built-in one.
    pub engine: Option<&'a dyn ContextEngine>,
    /// Trajectory events.
    pub events: &'a dyn EventSink,
    /// The running turn, when inside one.
    pub turn: Option<&'a str>,
}

/// What the read phase found.
pub(crate) struct Plan {
    pub(crate) rows: Vec<Row>,
    pub(crate) split: Split,
    pub(crate) before: u64,
}

/// The summary a compaction commits.
struct Written {
    document: String,
    coverage: Option<f64>,
    failure: Option<SummaryFailure>,
}

impl Compactor<'_> {
    /// Runs one attempt.
    ///
    /// # Errors
    ///
    /// Store failures; every compaction outcome is an [`Outcome`].
    pub async fn compact(&self, run: &Run) -> Result<Outcome, Error> {
        self.compact_with(run, None).await
    }

    /// [`Self::compact`] adopting `prepared` when the transcript only grew
    /// since it was prepared (spec: Prepared-ahead summaries); otherwise
    /// it is discarded.
    ///
    /// # Errors
    ///
    /// As [`Self::compact`].
    pub async fn compact_with(
        &self,
        run: &Run,
        prepared: Option<Prepared>,
    ) -> Result<Outcome, Error> {
        if run.preview {
            return self.preview(run);
        }
        let Some(lock) = self.lock()? else {
            return Ok(Outcome::of(Status::LockSkipped, 0));
        };
        let result = self.locked(run, &lock, prepared).await;
        self.unlock(&lock);
        result
    }

    /// The session's compaction lock, `None` while another holds it.
    pub(crate) fn lock(&self) -> Result<Option<Lease>, Error> {
        Ok(
            match self.store.acquire_lease(
                self.session,
                LeaseKind::Compaction,
                &Holder::current(),
            )? {
                Acquire::Granted(lock) => Some(lock),
                Acquire::Busy { .. } => None,
            },
        )
    }

    pub(crate) fn unlock(&self, lock: &Lease) {
        if let Err(err) = self.store.release_lease(lock) {
            tracing::warn!(error = %err, "compaction lock could not be released; it lapses at its expiry");
        }
    }

    pub(crate) fn plan(&self, run: &Run) -> Result<Plan, Error> {
        let rows = self.store.messages(self.session, Visibility::Active)?;
        let options = Options {
            first_compaction: !rows.iter().any(is_summary),
            protect_first_n: self.config.protect_first_n,
            tail_budget: tail_budget(self.config, run.window.context_window, run.trigger_tokens),
            min_tail_users: self.config.min_tail_user_messages,
            keep_exchanges: run.keep_exchanges,
        };
        let split = split(&rows, &options);
        let before = run.context_tokens.unwrap_or_else(|| rows_tokens(&rows));
        Ok(Plan {
            rows,
            split,
            before,
        })
    }

    fn preview(&self, run: &Run) -> Result<Outcome, Error> {
        let plan = self.plan(run)?;
        if !plan.split.has_work(&plan.rows) {
            return Ok(Outcome::of(Status::NothingToDo, plan.before));
        }
        let middle = &plan.rows[plan.split.middle.clone()];
        let middle_tokens = rows_tokens(middle);
        let budget = summary_budget(self.config, run.window.context_window, middle_tokens);
        Ok(Outcome {
            tokens_after: plan.before.saturating_sub(middle_tokens) + budget,
            summarized: middle.iter().map(|r| r.id).collect(),
            ..Outcome::of(Status::Preview, plan.before)
        })
    }

    async fn locked(
        &self,
        run: &Run,
        lock: &Lease,
        prepared: Option<Prepared>,
    ) -> Result<Outcome, Error> {
        let started = Instant::now();
        let mut plan = self.plan(run)?;
        if let Some(engine) = self.engine {
            if let Some(prepared) = prepared {
                prepared.discard();
            }
            return self.engine_run(engine, run, lock, &plan, started).await;
        }
        let prepared = prepared.and_then(|p| p.adopt(&mut plan));
        if !plan.split.has_work(&plan.rows) {
            self.back_off(run.trigger)?;
            return Ok(Outcome::of(Status::NothingToDo, plan.before));
        }
        let pruned = prune(&plan.rows, plan.split.middle.clone());
        let written = match self.written(run, &plan, &pruned, prepared).await? {
            Ok(written) => written,
            Err(outcome) => return Ok(outcome),
        };
        let assembled = assemble(&plan.rows, &plan.split, &written.document);
        self.commit(
            run,
            lock,
            &plan,
            (assembled, &pruned.originals),
            written,
            started,
        )
        .await
    }

    /// The summarizer's request for `plan` (its middle pruned).
    pub(crate) fn input(&self, run: &Run, plan: &Plan, pruned: &Pruned) -> Input {
        let middle = &plan.rows[plan.split.middle.clone()];
        let fresh: Vec<Row> = pruned
            .rows
            .iter()
            .filter(|r| !is_summary(r))
            .cloned()
            .collect();
        Input {
            transcript: summary::render(&fresh),
            previous: previous_summary(middle),
            focus: run.focus.clone(),
            budget: summary_budget(self.config, run.window.context_window, rows_tokens(middle)),
            missing: Vec::new(),
        }
    }

    /// The checked summary of `plan`'s middle (`prepared`: a summary
    /// already written for it), or the outcome that ends the attempt.
    async fn written(
        &self,
        run: &Run,
        plan: &Plan,
        pruned: &Pruned,
        prepared: Option<PreparedSummary>,
    ) -> Result<Result<Written, Outcome>, Error> {
        let middle = &plan.rows[plan.split.middle.clone()];
        self.started(run, plan, 1).await;
        let mut input = self.input(run, plan, pruned);
        let todos = TodoList::load(self.store, self.session)?;
        let pinned = pins::section(self.store, self.session)?;
        let reinjected = todos.reinjection();
        let document =
            |sections: &Sections| sections.document(reinjected.as_deref(), pinned.as_deref());
        let items = Items::of(middle);
        let checklist = Checklist::new(&items, &todos.open_items());
        // A prepared summary that failed is written again, as if there
        // were none.
        let first = match prepared {
            Some(prepared) => match prepared.await_summary().await {
                Ok(text) if !text.trim().is_empty() => Ok(text),
                _ => self.summarize(&input).await,
            },
            None => self.summarize(&input).await,
        };
        let (mut sections, failure) = match first {
            Ok(text) => {
                let mut sections = Sections::parse(&text);
                let missing = checklist.missing(&document(&sections));
                if checklist.coverage(&document(&sections)) < self.config.quality_min_coverage {
                    input.missing = missing;
                    self.started(run, plan, 2).await;
                    match self.summarize(&input).await {
                        Ok(retry) => sections = Sections::parse(&retry),
                        Err(err) => tracing::warn!(
                            reason = err.kind.as_str(),
                            "the summary retry failed; keeping the first summary: {}",
                            err.message
                        ),
                    }
                }
                (sections, None)
            }
            Err(failure) => {
                self.summary_failed(&failure).await?;
                if failure.kind.aborts() || self.config.abort_on_summary_failure {
                    return Ok(Err(Outcome {
                        failure: Some(failure),
                        ..Outcome::of(Status::Failed, plan.before)
                    }));
                }
                let previous = input.previous.as_deref();
                (
                    summary::fallback(previous, &pruned.lines, &items),
                    Some(failure),
                )
            }
        };
        let checked = document(&sections);
        let coverage = checklist.coverage(&checked);
        quality::preserve(&mut sections, &checklist.missing(&checked));
        Ok(Ok(Written {
            document: document(&sections),
            coverage: Some(coverage),
            failure,
        }))
    }

    /// A plugin engine's compaction of the whole active transcript.
    async fn engine_run(
        &self,
        engine: &dyn ContextEngine,
        run: &Run,
        lock: &Lease,
        plan: &Plan,
        started: Instant,
    ) -> Result<Outcome, Error> {
        self.started(run, plan, 1).await;
        let assembled = match engine.compact(&plan.rows).await {
            Ok(output) => super::engine::assembled(&plan.rows, output),
            Err(why) => Err(why),
        };
        let assembled = match assembled {
            Ok(assembled) => assembled,
            Err(why) => return self.invalid(plan, &why).await,
        };
        let document = assembled
            .items
            .iter()
            .filter_map(|item| match item {
                Item::New(row) => row.content.clone(),
                Item::Kept(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let written = Written {
            document,
            coverage: None,
            failure: None,
        };
        self.commit(run, lock, plan, (assembled, &[]), written, started)
            .await
    }

    /// Validates and commits `assembled`; an invalid result is discarded.
    async fn commit(
        &self,
        run: &Run,
        lock: &Lease,
        plan: &Plan,
        (assembled, originals): (Assembled<'_>, &[Vec<u8>]),
        written: Written,
        started: Instant,
    ) -> Result<Outcome, Error> {
        if let Err(why) = validate(&assembled) {
            return self.invalid(plan, &why).await;
        }
        let replaced = assembled.commit().0;
        let committed = match self.apply(lock, &assembled, originals)? {
            Ok(committed) => committed,
            Err(conflict) => {
                self.back_off(run.trigger)?;
                self.failed("conflict", &conflict).await;
                return Ok(Outcome::of(Status::Failed, plan.before));
            }
        };
        let after_rows = self.store.messages(self.session, Visibility::Active)?;
        let tokens_after = plan
            .before
            .saturating_sub(rows_tokens(&plan.rows).saturating_sub(rows_tokens(&after_rows)));
        let mut state = self.store.compression_state(self.session)?;
        // An overflow attempt ignores an armed cooldown without clearing it.
        let summarized = written.failure.is_none() && run.trigger != Trigger::Overflow;
        decision::committed(&mut state, plan.before, summarized);
        self.store.save_compression_state(self.session, &state)?;
        let outcome = Outcome {
            status: Status::Compressed,
            epoch: Some(committed.epoch),
            tokens_before: plan.before,
            tokens_after,
            summarized: replaced,
            fallback: written.failure.is_some(),
            failure: written.failure,
        };
        self.emit(Event::CompressionCommitted(CompressionCommitted {
            trigger: run.trigger.as_str().to_owned(),
            epoch: committed.epoch,
            tokens_before: outcome.tokens_before,
            tokens_after: outcome.tokens_after,
            summary_tokens: tokens(&written.document),
            coverage: written.coverage,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
        .await;
        Ok(outcome)
    }

    /// Commits an already validated `assembled` in place, keeping each of
    /// `originals` in the blob store for the row that names it; `Err`
    /// carries a concurrent rewrite (nothing changed).
    pub(crate) fn apply(
        &self,
        lock: &Lease,
        assembled: &Assembled<'_>,
        originals: &[Vec<u8>],
    ) -> Result<Result<Compacted, String>, Error> {
        let (replaced, replacements) = assembled.commit();
        let mut compaction = Compaction::new(assembled.source(), replaced, replacements);
        for original in originals {
            let blob = self.store.put_blob(original)?;
            compaction.references.push((blob, assembled.summary_id));
        }
        match self.store.compact(lock, &compaction) {
            Ok(committed) => {
                note_compaction(self.session);
                Ok(Ok(committed))
            }
            Err(err) if err.code().code == "store.compaction_conflict" => {
                Ok(Err(err.message().to_owned()))
            }
            Err(err) => Err(err),
        }
    }

    /// An engine result the host refused: nothing changes and it counts
    /// as a failed summary.
    async fn invalid(&self, plan: &Plan, why: &str) -> Result<Outcome, Error> {
        let failure = SummaryFailure::new(FailureKind::InvalidEngineOutput, why);
        self.summary_failed(&failure).await?;
        Ok(Outcome {
            failure: Some(failure),
            ..Outcome::of(Status::Failed, plan.before)
        })
    }

    /// The summary of `input`, under `compression.summary_timeout_secs`.
    pub(crate) async fn summarize(&self, input: &Input) -> Result<String, SummaryFailure> {
        self.summarize_request(&summary::request(input)).await
    }

    /// The summarizer's answer to `request`, under
    /// `compression.summary_timeout_secs`.
    pub(crate) async fn summarize_request(
        &self,
        request: &pmt_providers::request::Request,
    ) -> Result<String, SummaryFailure> {
        let Some(summarizer) = self.summarizer else {
            return Err(SummaryFailure::new(
                FailureKind::Failed,
                "no auxiliary compression route resolved",
            ));
        };
        let limit = Duration::from_secs(self.config.summary_timeout_secs);
        let text = tokio::time::timeout(limit, summarizer.summarize(request))
            .await
            .map_err(|_| {
                SummaryFailure::new(
                    FailureKind::Timeout,
                    format!("the summary did not finish within {} s", limit.as_secs()),
                )
            })??;
        if text.trim().is_empty() {
            return Err(SummaryFailure::new(
                FailureKind::Failed,
                "the summary was empty",
            ));
        }
        Ok(text)
    }

    /// Arms the cooldown and reports the failure.
    pub(crate) async fn summary_failed(&self, failure: &SummaryFailure) -> Result<(), Error> {
        let mut state = self.store.compression_state(self.session)?;
        let wait = decision::summary_failed(&mut state, Timestamp::now());
        self.store.save_compression_state(self.session, &state)?;
        tracing::warn!(
            reason = failure.kind.as_str(),
            cooldown_secs = wait.as_secs(),
            "compaction summary failed: {}",
            failure.message
        );
        self.failed(failure.kind.as_str(), &failure.message).await;
        Ok(())
    }

    /// An automatic attempt that committed nothing arms the backoff.
    fn back_off(&self, trigger: Trigger) -> Result<(), Error> {
        if trigger == Trigger::Manual {
            return Ok(());
        }
        let mut state = self.store.compression_state(self.session)?;
        decision::back_off(&mut state, Timestamp::now());
        self.store.save_compression_state(self.session, &state)
    }

    /// `compression.started` for summary attempt `attempt`.
    async fn started(&self, run: &Run, plan: &Plan, attempt: u32) {
        self.emit(Event::CompressionStarted(CompressionStarted {
            trigger: run.trigger.as_str().to_owned(),
            attempt,
            tokens_before: plan.before,
        }))
        .await;
    }

    pub(crate) async fn failed(&self, reason: &str, message: &str) {
        self.emit(Event::CompressionFailed(CompressionFailed {
            reason: reason.to_owned(),
            message: Some(message.to_owned()),
        }))
        .await;
    }

    async fn emit(&self, event: Event) {
        let kind = event.kind();
        let emit = match self.turn {
            Some(turn) => Emit::now(event).in_turn(turn, None),
            None => Emit::now(event),
        };
        if let Err(err) = self.events.emit(emit).await {
            tracing::debug!(event = kind, error = %err, "trajectory subscriber failed");
        }
    }
}

/// Identical file reads are answered in full again after a compaction
/// (the earlier answer may have been summarized away).
fn note_compaction(session: SessionId) {
    if let Some(files) = Environments::process()
        .session(&session.to_string())
        .and_then(|env| env.files.clone())
    {
        files.note_compaction();
    }
}
