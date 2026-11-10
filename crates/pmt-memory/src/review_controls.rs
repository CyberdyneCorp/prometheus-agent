//! Background review controls (spec: memory, Background review controls):
//! the replayed-input budget of one review, the extra tools it may borrow
//! from its parent session, its ledger purpose, one queued review per
//! session, and deferral of reviews on a local model until the machine is
//! idle.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_config::schema::{MemoryReviewConfig, ReviewDefer};
use pmt_core::error::{Error, codes};
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Request, Tool};

/// The ledger purpose of a review's model calls.
pub const PURPOSE: &str = "background_review";
/// The budget when the review model's window is unknown.
const UNKNOWN_WINDOW_BUDGET: u64 = 120_000;
/// The largest automatic budget.
const MAX_BUDGET: u64 = 600_000;

/// The replayed-input budget of one review: `max_input_tokens`, else 75%
/// of the review model's window (at most 600000; 120000 when the window is
/// unknown); `None` (unlimited) for 0 or a negative value.
#[must_use]
pub fn input_budget(config: &MemoryReviewConfig, window: Option<u64>) -> Option<u64> {
    match config.max_input_tokens {
        Some(n) if n <= 0 => None,
        Some(n) => Some(u64::try_from(n).unwrap_or(u64::MAX)),
        None => Some(window.map_or(UNKNOWN_WINDOW_BUDGET, |w| (w * 3 / 4).min(MAX_BUDGET))),
    }
}

/// The extra tools a review gets: the configured names the parent session
/// offers, in the parent's order.
#[must_use]
pub fn extra_tools(config: &MemoryReviewConfig, parent: &[Tool]) -> Vec<Tool> {
    parent
        .iter()
        .filter(|t| config.extra_tools.contains(&t.name))
        .cloned()
        .collect()
}

/// The review's model: every call is recorded with purpose
/// [`PURPOSE`], and a call that would bring the replayed input past the
/// budget is not sent (the review stops; the calls made so far stay in
/// the ledger).
pub struct ReviewModel {
    inner: Arc<dyn ModelPort>,
    budget: Option<u64>,
    sent: AtomicU64,
}

impl ReviewModel {
    /// `inner` under `budget` replayed input tokens.
    #[must_use]
    pub fn new(inner: Arc<dyn ModelPort>, budget: Option<u64>) -> Self {
        Self {
            inner,
            budget,
            sent: AtomicU64::new(0),
        }
    }

    /// Input tokens replayed so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
}

impl ModelPort for ReviewModel {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        self.inner.route()
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        self.inner.start_turn()
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        Box::pin(async move {
            let input = request.estimated_tokens();
            let total = self.sent() + input;
            if let Some(budget) = self.budget
                && total > budget
            {
                tracing::info!(total, budget, "memory review stopped at its input budget");
                return Err(ModelFailure::new(
                    Error::new(
                        &codes::BUDGET_EXCEEDED,
                        format!(
                            "the review's next call would replay {total} input tokens, over memory.review.max_input_tokens ({budget})"
                        ),
                    ),
                    None,
                ));
            }
            self.sent.store(total, Ordering::Relaxed);
            let mut ctx = ctx.clone();
            PURPOSE.clone_into(&mut ctx.purpose);
            self.inner.call(request, &ctx, deltas).await
        })
    }
}

/// Sessions with a review queued or running.
static QUEUED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// A session's place in the review queue: queued reviews coalesce per
/// session, so a second one while the first waits or runs is dropped.
pub struct Slot(String);

impl Slot {
    /// The slot of `session`, or `None` when it already has one.
    #[must_use]
    pub fn take(session: &str) -> Option<Self> {
        QUEUED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session.to_owned())
            .then(|| Self(session.to_owned()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        QUEUED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Global CPU use, in percent, under which the machine counts as idle.
const IDLE_CPU_PERCENT: f32 = 25.0;
/// How often a deferred review looks at the machine again.
const IDLE_POLL: Duration = Duration::from_secs(15);

/// Waits before a review on `route`: with `defer = auto` a review whose
/// model runs on a local inference server waits until the machine is
/// idle, at most `defer_max_age_secs`; anything else runs at once.
pub async fn wait_for_turn(config: &MemoryReviewConfig, route: &RouteInfo) {
    if config.defer != ReviewDefer::Auto || !route.local {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(config.defer_max_age_secs);
    let mut system = sysinfo::System::new();
    while Instant::now() < deadline {
        system.refresh_cpu_usage();
        tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
        system.refresh_cpu_usage();
        if system.global_cpu_usage() < IDLE_CPU_PERCENT {
            return;
        }
        tokio::time::sleep(IDLE_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max: Option<i64>) -> MemoryReviewConfig {
        MemoryReviewConfig {
            max_input_tokens: max,
            ..MemoryReviewConfig::default()
        }
    }

    #[test]
    fn the_budget_follows_the_window_and_zero_is_unlimited() {
        assert_eq!(input_budget(&config(None), Some(200_000)), Some(150_000));
        assert_eq!(input_budget(&config(None), Some(1_000_000)), Some(600_000));
        assert_eq!(input_budget(&config(None), None), Some(120_000));
        assert_eq!(input_budget(&config(Some(48_000)), None), Some(48_000));
        assert_eq!(input_budget(&config(Some(0)), None), None);
        assert_eq!(input_budget(&config(Some(-1)), None), None);
    }

    #[test]
    fn a_second_review_of_a_session_coalesces() {
        let first = Slot::take("ses_a").unwrap();
        assert!(Slot::take("ses_a").is_none());
        drop(first);
        assert!(Slot::take("ses_a").is_some());
    }
}
