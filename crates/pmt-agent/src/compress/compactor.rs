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
use pmt_obs::trajectory::payload::{CompressionCommitted, CompressionFailed};
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{Acquire, Compaction, Holder, Lease, LeaseKind, Message as Row, Store, Visibility};
use pmt_tools::environments::Environments;

use super::assemble::assemble;
use super::items::{Items, previous_summary};
use super::prune::prune;
use super::split::{Options, Split, split};
use super::summary::{self, FailureKind, Input, Sections, Summarizer, SummaryFailure};
use super::threshold::{ModelWindow, summary_budget, tail_budget};
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
    /// Trajectory events.
    pub events: &'a dyn EventSink,
    /// The running turn, when inside one.
    pub turn: Option<&'a str>,
}

/// What the read phase found.
struct Plan {
    rows: Vec<Row>,
    split: Split,
    before: u64,
}

impl Compactor<'_> {
    /// Runs one attempt.
    ///
    /// # Errors
    ///
    /// Store failures; every compaction outcome is an [`Outcome`].
    pub async fn compact(&self, run: &Run) -> Result<Outcome, Error> {
        if run.preview {
            return self.preview(run);
        }
        let lock = match self.store.acquire_lease(
            self.session,
            LeaseKind::Compaction,
            &Holder::current(),
        )? {
            Acquire::Granted(lock) => lock,
            Acquire::Busy { .. } => return Ok(Outcome::of(Status::LockSkipped, 0)),
        };
        let result = self.locked(run, &lock).await;
        if let Err(err) = self.store.release_lease(&lock) {
            tracing::warn!(error = %err, "compaction lock could not be released; it lapses at its expiry");
        }
        result
    }

    fn plan(&self, run: &Run) -> Result<Plan, Error> {
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

    async fn locked(&self, run: &Run, lock: &Lease) -> Result<Outcome, Error> {
        let started = Instant::now();
        let plan = self.plan(run)?;
        if !plan.split.has_work(&plan.rows) {
            self.back_off(run.trigger)?;
            return Ok(Outcome::of(Status::NothingToDo, plan.before));
        }
        let middle = &plan.rows[plan.split.middle.clone()];
        let pruned = prune(&plan.rows, plan.split.middle.clone());
        let fresh: Vec<Row> = pruned
            .rows
            .iter()
            .filter(|r| !is_summary(r))
            .cloned()
            .collect();
        let previous = previous_summary(middle);
        let input = Input {
            transcript: summary::render(&fresh),
            previous: previous.clone(),
            focus: run.focus.clone(),
            budget: summary_budget(self.config, run.window.context_window, rows_tokens(middle)),
        };
        let (sections, failure) = match self.summarize(&input).await {
            Ok(text) => (Sections::parse(&text), None),
            Err(failure) => {
                self.summary_failed(&failure).await?;
                if failure.kind.aborts() || self.config.abort_on_summary_failure {
                    return Ok(Outcome {
                        failure: Some(failure),
                        ..Outcome::of(Status::Failed, plan.before)
                    });
                }
                let items = Items::of(middle);
                (
                    summary::fallback(previous.as_deref(), &pruned.lines, &items),
                    Some(failure),
                )
            }
        };
        let todos = TodoList::load(self.store, self.session)?.reinjection();
        let document = sections.document(todos.as_deref());
        let assembled = assemble(&plan.rows, &plan.split, &document);
        let (replaced, replacements) = assembled.commit();
        let mut compaction = Compaction::new(&plan.rows, replaced.clone(), replacements);
        for original in &pruned.originals {
            let blob = self.store.put_blob(original)?;
            compaction.references.push((blob, assembled.summary_id));
        }
        let committed = match self.store.compact(lock, &compaction) {
            Ok(committed) => committed,
            Err(err) if err.code().code == "store.compaction_conflict" => {
                self.back_off(run.trigger)?;
                self.failed("conflict", err.message()).await;
                return Ok(Outcome::of(Status::Failed, plan.before));
            }
            Err(err) => return Err(err),
        };
        let after_rows = self.store.messages(self.session, Visibility::Active)?;
        let tokens_after = plan
            .before
            .saturating_sub(rows_tokens(&plan.rows).saturating_sub(rows_tokens(&after_rows)));
        let mut state = self.store.compression_state(self.session)?;
        // An overflow attempt ignores an armed cooldown without clearing it.
        let summarized = failure.is_none() && run.trigger != Trigger::Overflow;
        decision::committed(&mut state, plan.before, summarized);
        self.store.save_compression_state(self.session, &state)?;
        note_compaction(self.session);
        let outcome = Outcome {
            status: Status::Compressed,
            epoch: Some(committed.epoch),
            tokens_before: plan.before,
            tokens_after,
            summarized: replaced,
            fallback: failure.is_some(),
            failure,
        };
        self.emit(Event::CompressionCommitted(CompressionCommitted {
            trigger: run.trigger.as_str().to_owned(),
            epoch: committed.epoch,
            tokens_before: outcome.tokens_before,
            tokens_after: outcome.tokens_after,
            summary_tokens: tokens(&document),
            coverage: None,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
        .await;
        Ok(outcome)
    }

    /// The summary, under `compression.summary_timeout_secs`.
    async fn summarize(&self, input: &Input) -> Result<String, SummaryFailure> {
        let Some(summarizer) = self.summarizer else {
            return Err(SummaryFailure::new(
                FailureKind::Failed,
                "no auxiliary compression route resolved",
            ));
        };
        let request = summary::request(input);
        let limit = Duration::from_secs(self.config.summary_timeout_secs);
        let text = tokio::time::timeout(limit, summarizer.summarize(&request))
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
    async fn summary_failed(&self, failure: &SummaryFailure) -> Result<(), Error> {
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

    async fn failed(&self, reason: &str, message: &str) {
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
