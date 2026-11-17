//! What may start (spec: harness-evolution, Budget ledger and spend cap;
//! Concurrency limits; Job lifecycle): a job's [`JobGate`] stops new model
//! calls once the job is cancelled or its spend reaches the cap, and
//! cancels the token every in-flight rollout and session watches; the
//! process-wide [`Limits`] bound concurrent sessions and rollouts across
//! every job, and run one job per (profile, domain) at a time, FIFO.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use pmt_config::schema::EvolveConfig;
use pmt_core::cancel::CancelToken;
use tokio::sync::Semaphore;

use super::ledger::{Costs, Kind};

/// Why a job stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `evolve cancel`.
    Cancelled,
    /// The spend reached the cap.
    BudgetExceeded,
}

impl Stop {
    /// The job status it ends in.
    #[must_use]
    pub const fn status(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BudgetExceeded => "budget-exceeded",
        }
    }
}

#[derive(Debug, Default)]
struct State {
    costs: Costs,
    stop: Option<Stop>,
}

/// One job's spend and stop switch.
#[derive(Debug)]
pub struct JobGate {
    cancel: CancelToken,
    cap: f64,
    state: Mutex<State>,
}

impl JobGate {
    /// A gate that stops at `cap_usd`.
    #[must_use]
    pub fn new(cap_usd: f64) -> Self {
        Self {
            cancel: CancelToken::new(),
            cap: cap_usd,
            state: Mutex::new(State::default()),
        }
    }

    /// The token in-flight work watches.
    #[must_use]
    pub fn token(&self) -> &CancelToken {
        &self.cancel
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether new model calls may start.
    ///
    /// # Errors
    ///
    /// Why not.
    pub fn admit(&self) -> Result<(), Stop> {
        self.state().stop.map_or(Ok(()), Err)
    }

    /// Records `usd` of `kind`; reaching the cap stops the job.
    pub fn charge(&self, kind: Kind, usd: f64) {
        let mut state = self.state();
        state.costs.add(kind, usd);
        if state.stop.is_none() && self.cap > 0.0 && state.costs.total() >= self.cap {
            state.stop = Some(Stop::BudgetExceeded);
            drop(state);
            tracing::warn!(cap = self.cap, "evolution job reached its spend cap");
            self.cancel.cancel();
        }
    }

    /// Cancels the job.
    pub fn cancel(&self) {
        let mut state = self.state();
        if state.stop.is_none() {
            state.stop = Some(Stop::Cancelled);
        }
        drop(state);
        self.cancel.cancel();
    }

    /// Why the job stopped, if it did.
    #[must_use]
    pub fn stopped(&self) -> Option<Stop> {
        self.state().stop
    }

    /// The spend so far.
    #[must_use]
    pub fn costs(&self) -> Costs {
        self.state().costs
    }
}

/// Process-wide concurrency bounds.
#[derive(Debug)]
pub struct Limits {
    /// Improver sessions.
    pub sessions: Semaphore,
    /// Rollouts.
    pub rollouts: Semaphore,
    domains: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

static PROCESS: OnceLock<Arc<Limits>> = OnceLock::new();

impl Limits {
    /// Bounds of `sessions` and `rollouts`.
    #[must_use]
    pub fn new(sessions: u32, rollouts: u32) -> Self {
        let permits = |n: u32| usize::try_from(n.max(1)).unwrap_or(1);
        Self {
            sessions: Semaphore::new(permits(sessions)),
            rollouts: Semaphore::new(permits(rollouts)),
            domains: Mutex::new(HashMap::new()),
        }
    }

    /// This process's bounds, sized by the first configuration that asks.
    #[must_use]
    pub fn process(config: &EvolveConfig) -> Arc<Self> {
        Arc::clone(PROCESS.get_or_init(|| {
            Arc::new(Self::new(
                config.concurrency.sessions,
                config.concurrency.rollouts,
            ))
        }))
    }

    /// Waits for the turn of (profile, domain) `key`; jobs of one key run
    /// one at a time in arrival order (the lock is fair).
    pub async fn domain(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut domains = self
                .domains
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(domains.entry(key.to_owned()).or_default())
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaching_the_cap_stops_new_work_and_cancels_the_token() {
        let gate = JobGate::new(1.0);
        gate.charge(Kind::Rollout, 0.6);
        assert!(gate.admit().is_ok());
        gate.charge(Kind::Session, 0.4);
        assert_eq!(gate.admit(), Err(Stop::BudgetExceeded));
        assert!(gate.token().is_cancelled());
        assert!((gate.costs().total() - 1.0).abs() < 1e-9);
        gate.cancel();
        assert_eq!(
            gate.stopped(),
            Some(Stop::BudgetExceeded),
            "the first stop wins"
        );
    }
}
