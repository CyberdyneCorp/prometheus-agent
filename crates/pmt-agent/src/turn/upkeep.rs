//! Compaction upkeep around the turn loop's compaction (spec:
//! context-compression, Prepared-ahead summaries; Opt-in proactive prune;
//! Opt-in micro-compaction; Gateway session hygiene).
//!
//! Below the trigger, the pre-request check may write a summary ahead
//! ([`crate::compress::prepared`]) and run the proactive prune
//! ([`crate::compress::proactive`]); after a completed turn it may fold one
//! exchange ([`crate::compress::micro`]); before a gateway turn the gateway
//! asks for [`Agent::gateway_hygiene`]. Every commit here, like any
//! compaction, rebuilds the system prompt before the next request.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use pmt_config::{Config, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::id::MessageId;
use pmt_store::{Message as Row, Visibility};

use super::Agent;
use super::compression::window;
use super::model::RouteInfo;
use super::run::Turn;
use crate::compress::compactor::{Outcome, Run, Status};
use crate::compress::decision::Measure;
use crate::compress::hygiene::{self, Known};
use crate::compress::micro::{MAX_FAILURES, Micro};
use crate::compress::prepared::Prepared;
use crate::compress::proactive::{MIN_RECLAIM, Proactive};
use crate::compress::prune::prune;
use crate::compress::summary::{self, FailureKind, SummaryFailure};
use crate::compress::{Trigger, is_summary, rows_tokens};
use crate::prompt::frozen::{Frozen, Rebuild};

/// Micro-compaction progress of a session.
#[derive(Debug, Default)]
pub(super) struct MicroState {
    /// Completed turns since the agent started.
    completed: u32,
    /// Consecutive failures per exchange (by its first row).
    failures: HashMap<MessageId, u32>,
    /// Exchanges given up on.
    skipped: HashSet<MessageId>,
}

impl Agent {
    /// Gateway session hygiene before a gateway turn: compacts with trigger
    /// `gateway_hygiene` when the session's context reaches 85% of the
    /// window or 5,000 messages; `None` when it is not needed.
    ///
    /// # Errors
    ///
    /// `turn.scope_mismatch` for another profile's scope; store failures.
    pub async fn gateway_hygiene(&self, scope: &ProfileScope) -> Result<Option<Outcome>, Error> {
        if scope.dir() != self.setup.scope.dir() {
            return Err(Error::new(
                &codes::TURN_SCOPE_MISMATCH,
                format!(
                    "this agent runs a session of profile '{}'",
                    self.setup.scope.name()
                ),
            ));
        }
        let config = scope.config();
        if !config.compression.enabled {
            return Ok(None);
        }
        let rows = self
            .setup
            .store
            .messages(self.setup.session, Visibility::Active)?;
        let route = self.setup.model.route().await;
        let known = self.known_context(&rows)?;
        if !hygiene::needed(known, window(&route).context_window, rows.len()) {
            return Ok(None);
        }
        let mut run = Run::new(
            Trigger::GatewayHygiene,
            window(&route),
            self.trigger_tokens(config, &route),
        );
        run.context_tokens = Some(known.tokens());
        let outcome = self.compactor(config, None).compact(&run).await?;
        if outcome.status == Status::Compressed {
            self.committed();
            self.rebuild_prompt(Rebuild::Compaction);
        }
        Ok(Some(outcome))
    }

    /// The session's context: this agent's last reported count, the
    /// persisted usage of the last main call after the last compaction,
    /// or an estimate (each plus the rows added since).
    fn known_context(&self, rows: &[Row]) -> Result<Known, Error> {
        if let Some((tokens, counted)) = self.usage_memory().reported()
            && rows.len() >= counted
        {
            return Ok(Known::Reported(tokens + rows_tokens(&rows[counted..])));
        }
        let compacted = rows
            .iter()
            .filter(|r| is_summary(r))
            .map(|r| r.created_at)
            .max();
        let last = self
            .setup
            .store
            .usage_events(self.setup.session)?
            .into_iter()
            .rev()
            .find(|e| e.purpose == "main")
            .filter(|e| compacted.is_none_or(|at| e.ts > at));
        Ok(match last {
            Some(event) => {
                let t = event.tokens;
                let since: Vec<Row> = rows
                    .iter()
                    .filter(|r| r.created_at > event.ts)
                    .cloned()
                    .collect();
                Known::Persisted(
                    t.input + t.cache_read + t.cache_write + t.output + rows_tokens(&since),
                )
            }
            None => Known::Estimated(rows_tokens(rows)),
        })
    }

    /// The summary prepared ahead, if any.
    pub(super) fn take_prepared(&self) -> Option<Prepared> {
        self.prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// After a completed turn: one micro-compaction pass when it is due.
    /// A failure is logged; it never fails the turn.
    pub(super) async fn after_completed_turn(&self, config: &Config) {
        let compression = &config.compression;
        if !compression.micro_compact {
            return;
        }
        let due = {
            let mut micro = self.micro_state();
            micro.completed += 1;
            micro
                .completed
                .is_multiple_of(compression.micro_compact_every_n_turns.max(1))
        };
        if due && let Err(err) = self.micro_pass(config).await {
            tracing::warn!(error = %err, "micro-compaction failed");
        }
    }

    fn micro_state(&self) -> std::sync::MutexGuard<'_, MicroState> {
        self.micro
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn micro_pass(&self, config: &Config) -> Result<(), Error> {
        let route = self.setup.model.route().await;
        // The trigger only sizes the protected tail here.
        let run = Run::new(
            Trigger::PostTool,
            window(&route),
            self.trigger_tokens(config, &route),
        );
        let skip = self.micro_state().skipped.clone();
        match self
            .compactor(config, None)
            .micro_compact(&run, &skip)
            .await?
        {
            Micro::Folded(first) => {
                self.micro_state().failures.remove(&first);
                self.committed();
                self.rebuild_prompt(Rebuild::Compaction);
            }
            Micro::Failed(first, why) => {
                tracing::warn!(reason = %why, "micro-compaction could not fold an exchange");
                let mut micro = self.micro_state();
                let failures = micro.failures.entry(first).or_default();
                *failures += 1;
                if *failures >= MAX_FAILURES {
                    micro.skipped.insert(first);
                }
            }
            Micro::Nothing | Micro::Busy => {}
        }
        Ok(())
    }
}

impl Turn<'_> {
    /// Below the compaction trigger: prepare a summary ahead and run the
    /// proactive prune, each when configured and due. Only a reported
    /// count (or the estimate of a provider that omits usage) counts.
    pub(super) async fn below_trigger(
        &mut self,
        frozen: &mut Frozen,
        route: &RouteInfo,
        measure: Measure,
        omits_usage: bool,
    ) -> Result<(), Error> {
        let tokens = match measure {
            Measure::Reported(tokens) => tokens,
            Measure::Estimated(tokens) if omits_usage => tokens,
            Measure::Estimated(_) => return Ok(()),
        };
        self.prepare_ahead(route, tokens)?;
        self.proactive_prune(frozen, route, tokens).await
    }

    /// Starts writing the summary in the background once the context
    /// passes 90% of the trigger.
    fn prepare_ahead(&self, route: &RouteInfo, tokens: u64) -> Result<(), Error> {
        let agent = self.agent;
        let config = &self.config.compression;
        let threshold = agent.trigger_tokens(self.config, route);
        let Some(summarizer) = agent.setup.summarizer.clone() else {
            return Ok(());
        };
        let near = tokens.saturating_mul(10) >= threshold.saturating_mul(9);
        if !config.prepare_ahead || !near || agent.setup.context_engine.is_some() {
            return Ok(());
        }
        let mut slot = agent
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Ok(());
        }
        let run = Run::new(Trigger::PreRequest, window(route), threshold);
        let compactor = agent.compactor(self.config, None);
        let plan = compactor.plan(&run)?;
        if !plan.split.has_work(&plan.rows) {
            return Ok(());
        }
        let pruned = prune(&plan.rows, plan.split.middle.clone());
        let request = summary::request(&compactor.input(&run, &plan, &pruned));
        let limit = Duration::from_secs(config.summary_timeout_secs);
        let task = tokio::spawn(async move {
            tokio::time::timeout(limit, summarizer.summarize(&request))
                .await
                .unwrap_or_else(|_| {
                    Err(SummaryFailure::new(
                        FailureKind::Timeout,
                        "the prepared summary did not finish in time",
                    ))
                })
        });
        tracing::debug!(tokens, threshold, "writing the compaction summary ahead");
        *slot = Some(Prepared::new(&plan.rows, plan.split, task));
        Ok(())
    }

    /// The proactive prune, when the context passed
    /// `compression.proactive_prune_tokens` and it is armed.
    async fn proactive_prune(
        &mut self,
        frozen: &mut Frozen,
        route: &RouteInfo,
        tokens: u64,
    ) -> Result<(), Error> {
        let at = self.config.compression.proactive_prune_tokens;
        let rearm = self.agent.usage_memory().proactive_rearm;
        if at == 0 || tokens < at || rearm.is_some_and(|r| tokens < r) {
            return Ok(());
        }
        let threshold = self.agent.trigger_tokens(self.config, route);
        let run = Run::new(Trigger::PreRequest, window(route), threshold);
        let turn = self.id.to_string();
        let result = self
            .agent
            .compactor(self.config, Some(&turn))
            .proactive_prune(&run)?;
        // Committed: armed again once the context regrew by what it
        // reclaimed; too little: once it grew enough to be worth it.
        let next = match result {
            Proactive::Committed(_) => tokens,
            Proactive::TooLittle(_) => tokens + MIN_RECLAIM,
            Proactive::Skipped => return Ok(()),
        };
        self.agent.usage_memory().proactive_rearm = Some(next);
        if let Proactive::Committed(reclaimed) = result {
            tracing::info!(tokens, reclaimed, "proactive prune committed");
            self.agent.committed();
            self.agent.rebuild_prompt(Rebuild::Compaction);
            *frozen = self.freeze(route).await?;
        }
        Ok(())
    }
}
