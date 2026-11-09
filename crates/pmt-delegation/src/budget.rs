//! Child budgets (spec: subagent-delegation, Fan-out budget split and
//! roll-up; One-shot spawn budget).
//!
//! A call-level budget is divided across the call's tasks (evenly or by
//! `weight`) and each share is a hard limit on its child; shares are
//! scaled down so their sum never exceeds what the root lineage has left.
//! A child's spend is recorded under its own session, a descendant of the
//! parent's, so every usage total of the root lineage rolls it up.

use std::sync::atomic::{AtomicU32, Ordering};

use pmt_core::error::codes;

use crate::Refusal;
use crate::contract::{CallBudget, Split, Task};

/// One child's limits.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChildBudget {
    /// USD the child may spend.
    pub max_cost_usd: Option<f64>,
    /// Tokens the child may use.
    pub max_tokens: Option<u64>,
    /// Model calls the child may make.
    pub max_iterations: u32,
}

/// What the root lineage has left (`None`: no limit).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Remaining {
    /// USD left.
    pub usd: Option<f64>,
    /// Tokens left.
    pub tokens: Option<u64>,
}

/// The budgets of `tasks` under `call` (shares) and `remaining`; each
/// child makes at most `iterations[i]` model calls besides any share.
#[must_use]
pub fn split(
    call: Option<&CallBudget>,
    tasks: &[Task],
    iterations: &[u32],
    remaining: Remaining,
) -> Vec<ChildBudget> {
    let weights = weights(call.map_or(Split::Even, |c| c.split), tasks);
    let total: f64 = weights.iter().sum();
    let share = |whole: Option<f64>, cap: Option<f64>| -> Vec<Option<f64>> {
        let whole = match (whole, cap) {
            (Some(w), Some(c)) => Some(w.min(c)),
            (w, _) => w,
        };
        weights
            .iter()
            .map(|w| whole.map(|v| v * w / total))
            .collect()
    };
    #[allow(clippy::cast_precision_loss)] // token counts far below 2^52
    let tokens = share(
        call.and_then(|c| c.max_tokens).map(|t| t as f64),
        remaining.tokens.map(|t| t as f64),
    );
    let usd = share(call.and_then(|c| c.max_cost_usd), remaining.usd);
    let calls = share(call.and_then(|c| c.max_iterations).map(f64::from), None);
    (0..tasks.len())
        .map(|i| ChildBudget {
            max_cost_usd: usd[i],
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // non-negative shares
            max_tokens: tokens[i].map(|t| t.floor() as u64),
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // non-negative shares
            max_iterations: calls[i].map_or(iterations[i], |c| {
                (c.floor() as u32).clamp(1, iterations[i])
            }),
        })
        .collect()
}

fn weights(split: Split, tasks: &[Task]) -> Vec<f64> {
    tasks
        .iter()
        .map(|t| match split {
            Split::Even => 1.0,
            Split::Weighted => t.weight.unwrap_or(1.0),
        })
        .collect()
}

/// The spawn budget of a finite one-shot run (`chat -z`, cron, eval):
/// `delegation.oneshot_max_children` children in all (`0`: unlimited).
#[derive(Debug)]
pub struct OneShotBudget {
    max: u32,
    used: AtomicU32,
}

impl OneShotBudget {
    /// A budget of `max` children.
    #[must_use]
    pub const fn new(max: u32) -> Self {
        Self {
            max,
            used: AtomicU32::new(0),
        }
    }

    /// Charges `n` children.
    ///
    /// # Errors
    ///
    /// `delegation.budget_exhausted` when that would exceed the budget
    /// (nothing is charged).
    pub fn charge(&self, n: u32) -> Result<(), Refusal> {
        if self.max == 0 {
            return Ok(());
        }
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used + n <= self.max).then_some(used + n)
            })
            .map(drop)
            .map_err(|used| {
                Refusal::new(
                    &codes::DELEGATION_BUDGET_EXHAUSTED,
                    format!(
                        "Delegation budget for this run is exhausted ({used}/{} subagents used; \
                         delegation.oneshot_max_children).",
                        self.max
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    fn tasks(weights: &[f64]) -> Vec<Task> {
        weights
            .iter()
            .enumerate()
            .map(|(index, w)| Task {
                index,
                goal: "g".into(),
                context: None,
                agent: None,
                toolsets: None,
                model: None,
                output_schema: None,
                images: Vec::new(),
                group: None,
                weight: Some(*w),
                router_params: Map::new(),
            })
            .collect()
    }

    /// Scenario: Even split enforced (the shares).
    #[test]
    fn an_even_split_gives_each_child_a_quarter() {
        let call = CallBudget {
            max_cost_usd: Some(1.0),
            ..CallBudget::default()
        };
        let budgets = split(
            Some(&call),
            &tasks(&[1.0; 4]),
            &[150; 4],
            Remaining::default(),
        );
        assert!(budgets.iter().all(|b| b.max_cost_usd == Some(0.25)));
        assert!(budgets.iter().all(|b| b.max_iterations == 150));
    }

    #[test]
    fn weighted_shares_follow_weights_and_never_exceed_the_root() {
        let call = CallBudget {
            max_cost_usd: Some(1.0),
            max_tokens: Some(1000),
            max_iterations: Some(10),
            split: Split::Weighted,
        };
        let remaining = Remaining {
            usd: Some(0.5),
            tokens: None,
        };
        let budgets = split(Some(&call), &tasks(&[3.0, 1.0]), &[150, 4], remaining);
        assert_eq!(budgets[0].max_cost_usd, Some(0.375));
        assert_eq!(budgets[1].max_cost_usd, Some(0.125));
        assert_eq!(budgets[0].max_tokens, Some(750));
        assert_eq!(budgets[0].max_iterations, 7);
        assert_eq!(budgets[1].max_iterations, 2);
    }

    /// Scenario: Third child in a one-shot run.
    #[test]
    fn a_one_shot_run_refuses_the_third_child() {
        let budget = OneShotBudget::new(2);
        budget.charge(2).unwrap();
        let err = budget.charge(1).unwrap_err();
        assert_eq!(err.code.code, "delegation.budget_exhausted");
        assert_eq!(
            err.message,
            "Delegation budget for this run is exhausted (2/2 subagents used; delegation.oneshot_max_children)."
        );
        OneShotBudget::new(0).charge(99).unwrap();
    }
}
