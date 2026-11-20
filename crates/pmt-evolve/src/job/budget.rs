//! The recursion budget and stage labels (spec: harness-evolution, Modes,
//! budget and labels).
//!
//! A budget k fixes the whole search: every mode spends exactly R = 4k
//! rollouts and P = 3k improver sessions. Base rollouts `base1..basek` are
//! shared by the modes of one job. Parallel mode runs the 2k proposal
//! stages `inv<i>`/`dom<i>`, then k compositions `agg<i>`; sequential mode
//! runs k rounds of `inv<t>`, `dom<t>` and the checkpoint `c<t>`. Each
//! stage and composition is one improver session and one rollout.

use pmt_config::schema::EvolveMode;
use pmt_core::error::{Error, codes};
use serde::Serialize;

/// Smallest k.
pub const MIN_K: u8 = 1;
/// Largest k.
pub const MAX_K: u8 = 8;

/// Checks a `--k` value.
///
/// # Errors
///
/// `cli.usage` (exit 2) with `--k must be between 1 and 8`.
pub fn check_k(k: u8) -> Result<u8, Error> {
    if (MIN_K..=MAX_K).contains(&k) {
        Ok(k)
    } else {
        Err(Error::new(
            &codes::CLI_USAGE,
            format!("--k must be between {MIN_K} and {MAX_K}"),
        ))
    }
}

/// What one step of a mode is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A proposal stage with general edits (`inv<i>`).
    Invariant,
    /// A proposal stage with domain edits (`dom<i>`).
    Domain,
    /// A parallel composition (`agg<i>`): a final.
    Composition,
    /// A sequential checkpoint (`c<t>`): a final.
    Checkpoint,
}

/// One improver session and the rollout of its tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    /// The label (`inv1`, `agg2`, `c1`...).
    pub label: String,
    /// What it is.
    pub kind: StepKind,
}

impl Step {
    fn new(kind: StepKind, prefix: &str, i: u8) -> Self {
        Self {
            label: format!("{prefix}{i}"),
            kind,
        }
    }

    /// Whether the step's tree is one of the mode's finals.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        matches!(self.kind, StepKind::Composition | StepKind::Checkpoint)
    }
}

/// The steps of one mode, in the order a serial run takes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModePlan {
    /// `parallel` or `sequential`.
    pub mode: &'static str,
    /// The shared base rollouts.
    pub base: Vec<String>,
    /// Stages, compositions and checkpoints.
    pub steps: Vec<Step>,
}

impl ModePlan {
    /// Rollouts the mode spends: base rollouts plus one per step (R).
    #[must_use]
    pub fn rollouts(&self) -> usize {
        self.base.len() + self.steps.len()
    }

    /// Improver sessions the mode spends: one per step (P).
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.steps.len()
    }

    /// The proposal stages (`inv<i>`, `dom<i>`), in order.
    #[must_use]
    pub fn stages(&self) -> Vec<&str> {
        self.labels(|s| !s.is_final())
    }

    /// The finals (`agg<i>` or `c<t>`), in order.
    #[must_use]
    pub fn finals(&self) -> Vec<&str> {
        self.labels(Step::is_final)
    }

    fn labels(&self, keep: impl Fn(&Step) -> bool) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| keep(s))
            .map(|s| s.label.as_str())
            .collect()
    }
}

fn base(k: u8) -> Vec<String> {
    (1..=k).map(|i| format!("base{i}")).collect()
}

/// Parallel mode at budget `k`.
#[must_use]
pub fn parallel(k: u8) -> ModePlan {
    let mut steps = Vec::new();
    for i in 1..=k {
        steps.push(Step::new(StepKind::Invariant, "inv", i));
        steps.push(Step::new(StepKind::Domain, "dom", i));
    }
    steps.extend((1..=k).map(|i| Step::new(StepKind::Composition, "agg", i)));
    ModePlan {
        mode: "parallel",
        base: base(k),
        steps,
    }
}

/// Sequential mode at budget `k`.
#[must_use]
pub fn sequential(k: u8) -> ModePlan {
    let mut steps = Vec::new();
    for t in 1..=k {
        steps.push(Step::new(StepKind::Invariant, "inv", t));
        steps.push(Step::new(StepKind::Domain, "dom", t));
        steps.push(Step::new(StepKind::Checkpoint, "c", t));
    }
    ModePlan {
        mode: "sequential",
        base: base(k),
        steps,
    }
}

/// The modes a job runs (both share the base rollouts).
#[must_use]
pub fn plans(mode: EvolveMode, k: u8) -> Vec<ModePlan> {
    match mode {
        EvolveMode::Parallel => vec![parallel(k)],
        EvolveMode::Sequential => vec![sequential(k)],
        EvolveMode::Both => vec![parallel(k), sequential(k)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Parallel k=2 labels.
    #[test]
    fn parallel_k2_has_the_spec_stage_and_composition_labels() {
        let plan = parallel(2);
        assert_eq!(plan.stages(), ["inv1", "dom1", "inv2", "dom2"]);
        assert_eq!(plan.finals(), ["agg1", "agg2"]);
        assert_eq!(plan.base, ["base1", "base2"]);
    }

    #[test]
    fn sequential_rounds_end_in_their_checkpoint() {
        let labels: Vec<_> = sequential(2).steps.into_iter().map(|s| s.label).collect();
        assert_eq!(labels, ["inv1", "dom1", "c1", "inv2", "dom2", "c2"]);
    }

    /// Every mode at every k spends R = 4k rollouts and P = 3k sessions,
    /// with no label repeated (scenario: Parallel ledger counts, k=3).
    #[test]
    fn every_mode_spends_four_k_rollouts_and_three_k_sessions() {
        for k in MIN_K..=MAX_K {
            for plan in [parallel(k), sequential(k)] {
                let k = usize::from(k);
                assert_eq!(plan.rollouts(), 4 * k, "{} k={k}", plan.mode);
                assert_eq!(plan.sessions(), 3 * k, "{} k={k}", plan.mode);
                assert_eq!(plan.finals().len(), k);
                let mut labels: Vec<_> = plan.steps.iter().map(|s| &s.label).collect();
                labels.sort();
                labels.dedup();
                assert_eq!(labels.len(), plan.steps.len());
            }
        }
        let k3 = parallel(3);
        assert_eq!((k3.rollouts(), k3.sessions()), (12, 9));
        let both = plans(EvolveMode::Both, 2);
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].base, both[1].base, "base rollouts are shared");
    }

    /// Scenario: Invalid k.
    #[test]
    fn k_outside_one_to_eight_is_a_usage_error() {
        for bad in [0, 9] {
            let err = check_k(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2);
            assert_eq!(err.message(), "--k must be between 1 and 8");
        }
        assert_eq!(check_k(8).unwrap(), 8);
    }
}
