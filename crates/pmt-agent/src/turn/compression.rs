//! Compaction inside the turn loop (spec: context-compression, Trigger
//! points and attempt budget; Provider usage beats rough estimates;
//! Context overflow recovery; Compaction is the only sanctioned cache
//! break; Manual compaction command).
//!
//! Before each model request the loop measures the context: the provider's
//! reported input of the latest request plus estimates of the rows added
//! since, or, before any reported usage, a rough estimate of the whole
//! request (which triggers only for a provider known to omit usage; any
//! other provider gets the request sent uncompressed and is judged on its
//! reported usage). Over the trigger, and with the guards clear, it
//! compacts with the label of the point it is at (`turn_start` on the
//! first request, `post_tool` after a tool round, `pre_request`
//! otherwise). An overflow error compacts once with trigger `overflow`
//! regardless of an armed cooldown and retries the request. Every attempt
//! that reaches the compactor counts against `compression.max_attempts`
//! for the turn; none consumes an iteration. A commit rebuilds the system
//! prompt (`prompt.rebuilt`, reason `compaction`) before the next request.

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_store::{Message as Row, Visibility};

use super::Agent;
use super::exit::ExitReason;
use super::model::{ModelReply, RouteInfo};
use super::run::{Ending, Turn};
use crate::compress::compactor::{Compactor, Outcome, Run, Status};
use crate::compress::decision::{self, Blocked, Decision, Gate, Measure};
use crate::compress::manual::Manual;
use crate::compress::threshold::{ModelWindow, trigger_tokens};
use crate::compress::{Trigger, rows_tokens};
use crate::prompt::frozen::{Frozen, Rebuild};

/// The window assumed when the route reports none.
const FALLBACK_WINDOW: u64 = 128_000;

/// What the turn ends with when the context cannot be made to fit.
pub const CONTEXT_FULL: &str = "Context is full. Run /compress here 2 to keep only the last exchanges, or /new to start a new session.";

/// The provider's reported context size at the latest request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Anchor {
    /// Input (cache included) plus output tokens of the latest call.
    tokens: u64,
    /// Active rows the request was built from, plus the reply's row.
    rows: usize,
}

/// What the agent learned about the provider's usage reports.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct UsageMemory {
    pub(super) anchor: Option<Anchor>,
    /// A reply came back without usage.
    pub(super) omits_usage: bool,
    /// The persisted breaker state may await a judgement (set at start and
    /// after each commit).
    pub(super) judge: bool,
}

impl UsageMemory {
    pub(super) fn new() -> Self {
        Self {
            judge: true,
            ..Self::default()
        }
    }
}

/// The main model's window on `route`.
fn window(route: &RouteInfo) -> ModelWindow {
    let context_window = route.context_window.unwrap_or(FALLBACK_WINDOW);
    ModelWindow {
        context_window,
        reserved_output: route.max_output_tokens.unwrap_or(0).min(context_window / 2),
    }
}

impl Agent {
    fn trigger_tokens(&self, config: &pmt_config::Config, route: &RouteInfo) -> u64 {
        let compressor = self
            .setup
            .summarizer
            .as_ref()
            .and_then(|s| s.context_window());
        trigger_tokens(&config.compression, window(route), compressor)
    }

    fn compactor<'a>(
        &'a self,
        config: &'a pmt_config::Config,
        turn: Option<&'a str>,
    ) -> Compactor<'a> {
        Compactor {
            store: &self.setup.store,
            session: self.setup.session,
            config: &config.compression,
            summarizer: self.setup.summarizer.as_deref(),
            events: &*self.setup.events,
            turn,
        }
    }

    /// `/compress` (parsed with [`crate::compress::manual::parse`]) on
    /// this agent's session, between turns: clears the guards, compacts
    /// (or previews) and makes the next turn rebuild the system prompt.
    ///
    /// # Errors
    ///
    /// `turn.scope_mismatch` for another profile's scope; store failures.
    pub async fn compress(&self, scope: &ProfileScope, manual: &Manual) -> Result<Outcome, Error> {
        if scope.dir() != self.setup.scope.dir() {
            return Err(Error::new(
                &codes::TURN_SCOPE_MISMATCH,
                format!(
                    "this agent runs a session of profile '{}', not of '{}'",
                    self.setup.scope.name(),
                    scope.name()
                ),
            ));
        }
        let config = scope.config();
        if !manual.preview {
            let mut state = self.setup.store.compression_state(self.setup.session)?;
            decision::manual(&mut state);
            self.setup
                .store
                .save_compression_state(self.setup.session, &state)?;
        }
        let route = self.setup.model.route().await;
        let mut run = Run::new(
            Trigger::Manual,
            window(&route),
            self.trigger_tokens(config, &route),
        );
        run.focus.clone_from(&manual.focus);
        run.keep_exchanges = manual.keep_exchanges;
        run.preview = manual.preview;
        let outcome = self.compactor(config, None).compact(&run).await?;
        if outcome.status == Status::Compressed {
            self.committed();
            self.rebuild_prompt(Rebuild::Compaction);
        }
        Ok(outcome)
    }

    /// After a commit: the reported usage no longer describes the rows.
    fn committed(&self) {
        let mut memory = self.usage_memory();
        memory.anchor = None;
        memory.judge = true;
    }
}

impl Turn<'_> {
    /// The context size before the next request.
    fn measure(&self, rows: &[Row], estimate: u64) -> Measure {
        let anchor = self.agent.usage_memory().anchor;
        match anchor {
            Some(a) if rows.len() >= a.rows => {
                Measure::Reported(a.tokens + rows_tokens(&rows[a.rows..]))
            }
            _ => Measure::Estimated(estimate),
        }
    }

    /// The compression preflight before a request; `estimate` is the
    /// request's rough size. A commit refreezes `frozen`.
    pub(super) async fn preflight(
        &mut self,
        frozen: &mut Frozen,
        route: &RouteInfo,
        estimate: u64,
    ) -> Result<(), Error> {
        let trigger = std::mem::replace(&mut self.next_trigger, Trigger::PreRequest);
        if !self.config.compression.enabled
            || self.compression_attempts >= self.config.compression.max_attempts
        {
            return Ok(());
        }
        let setup = &self.agent.setup;
        let rows = setup.store.messages(setup.session, Visibility::Active)?;
        let threshold = self.agent.trigger_tokens(self.config, route);
        let omits = self.agent.usage_memory().omits_usage;
        let tokens = match decision::gate(self.measure(&rows, estimate), omits, threshold) {
            Gate::Send | Gate::MeasureFirst => return Ok(()),
            Gate::Evaluate(tokens) => tokens,
        };
        let state = setup.store.compression_state(setup.session)?;
        match decision::should_compress(tokens, threshold, &state, Timestamp::now()) {
            Decision::Run => {}
            other => {
                tracing::debug!(decision = %other, tokens, threshold, "compaction not run");
                return Ok(());
            }
        }
        self.attempt(trigger, frozen, route, Some(tokens))
            .await
            .map(drop)
    }

    /// One counted attempt; a commit refreezes `frozen`.
    async fn attempt(
        &mut self,
        trigger: Trigger,
        frozen: &mut Frozen,
        route: &RouteInfo,
        tokens: Option<u64>,
    ) -> Result<Outcome, Error> {
        let mut run = Run::new(
            trigger,
            window(route),
            self.agent.trigger_tokens(self.config, route),
        );
        run.context_tokens = tokens;
        let turn = self.id.to_string();
        let outcome = self
            .agent
            .compactor(self.config, Some(&turn))
            .compact(&run)
            .await?;
        if outcome.status != Status::LockSkipped {
            self.compression_attempts += 1;
        }
        if outcome.status == Status::Compressed {
            self.agent.committed();
            self.agent.rebuild_prompt(Rebuild::Compaction);
            *frozen = self.freeze(route).await?;
        }
        Ok(outcome)
    }

    /// An overflow: compact once (an armed cooldown does not hold it) and
    /// let the caller retry, or end the turn with `context_exhausted`.
    pub(super) async fn overflow(
        &mut self,
        frozen: &mut Frozen,
        route: &RouteInfo,
    ) -> Result<Option<Ending>, Error> {
        let setup = &self.agent.setup;
        let state = setup.store.compression_state(setup.session)?;
        let held = match decision::should_compress(1, 0, &state, Timestamp::now()) {
            Decision::Blocked(Blocked::Cooldown(_)) | Decision::Run => false,
            Decision::Blocked(_) | Decision::BelowThreshold => true,
        };
        let allowed = self.config.compression.enabled
            && self.compression_attempts < self.config.compression.max_attempts
            && !held;
        if allowed
            && self
                .attempt(Trigger::Overflow, frozen, route, None)
                .await?
                .status
                == Status::Compressed
        {
            return Ok(None);
        }
        Ok(Some(Ending::failed(
            ExitReason::ContextExhausted,
            Error::new(&codes::TURN_CONTEXT_EXHAUSTED, CONTEXT_FULL),
        )))
    }

    /// Learns from a reply's usage: the anchor of the next measure,
    /// whether the provider omits usage, and the judgement of the latest
    /// commit (the `ineffective` breaker).
    pub(super) fn observe(&self, reply: &ModelReply) -> Result<(), Error> {
        let usage = reply.usage;
        let input = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
        let judge = {
            let mut memory = self.agent.usage_memory();
            if input == 0 && usage.output_tokens == 0 {
                memory.omits_usage = true;
                memory.anchor = None;
                return Ok(());
            }
            memory.anchor = Some(Anchor {
                tokens: input + usage.output_tokens,
                rows: self.request_rows + 1,
            });
            std::mem::take(&mut memory.judge)
        };
        if judge {
            let setup = &self.agent.setup;
            let mut state = setup.store.compression_state(setup.session)?;
            if decision::observe_usage(&mut state, input) {
                setup.store.save_compression_state(setup.session, &state)?;
            }
        }
        Ok(())
    }
}
