//! The two search modes (spec: harness-evolution, Parallel mode;
//! Sequential mode). Each step is an improver session plus one rollout of
//! the tree it produced (a fallen-back stage is rolled out too). Steps
//! that do not depend on each other run concurrently under the process
//! limits; their ledger entries are appended in serial order.

use std::collections::BTreeSet;

use futures_util::future::join_all;
use pmt_config::harness::Tree;

use super::evidence::{self, PoolItem};
use super::gate::Stop;
use super::ledger::{Entry, Kind, ModeLedger};
use super::run::{RolloutDone, Run};
use super::stage::{StageResult, StageSpec};
use super::summary::{Similarity, StageSummary};
use crate::job::budget::{ModePlan, Step, StepKind};

/// A rolled-out step.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub stage: StageResult,
    /// The tree its diff is taken against (its fallback).
    pub base: Tree,
    pub rollout: RolloutDone,
}

/// What a mode produced.
#[derive(Debug, Default)]
pub(crate) struct ModeRun {
    pub ledger: ModeLedger,
    pub finals: Vec<Candidate>,
    pub stopped: Option<Stop>,
    pub similarity: Option<Similarity>,
}

/// The evidence view of finished base rollouts.
fn base_view(base: &[RolloutDone]) -> Vec<evidence::Rollout<'_>> {
    base.iter()
        .map(|r| evidence::Rollout {
            label: &r.label,
            outcome: &r.outcome,
            images: Some(&r.images),
        })
        .collect()
}

fn pool_view(candidates: &[Candidate]) -> Vec<PoolItem<'_>> {
    candidates
        .iter()
        .map(|c| PoolItem {
            label: &c.stage.label,
            tree: &c.stage.tree,
            diff_base: &c.base,
            rollout: &c.rollout.outcome,
            decision: &c.stage.decision,
        })
        .collect()
}

impl Run<'_> {
    /// Ledger entries of the shared base rollouts: charged in the first
    /// mode, free (cached) in a second.
    pub(crate) fn base_entries(ledger: &mut ModeLedger, base: &[RolloutDone], shared: bool) {
        for rollout in base {
            ledger.push(Entry {
                kind: Kind::Rollout,
                label: rollout.label.clone(),
                usd: if shared || rollout.cached {
                    0.0
                } else {
                    rollout.outcome.usd
                },
                cached: shared || rollout.cached,
                attempts: 1,
            });
        }
    }

    /// One step: its session(s), then its rollout.
    async fn step(&self, spec: StageSpec<'_>) -> Candidate {
        let stage = self.stage(&spec).await;
        let store = self.dir.join("modes").join(spec.mode).join(spec.label);
        let rollout = self.rollout(spec.label, &stage.tree, &store).await;
        Candidate {
            stage,
            base: spec.fallback.clone(),
            rollout,
        }
    }

    /// Records finished steps in the ledger and the summary, in order.
    fn record(&self, mode: &str, ledger: &mut ModeLedger, steps: &[Candidate]) {
        for c in steps {
            ledger.push(Entry {
                kind: Kind::Session,
                label: c.stage.label.clone(),
                usd: c.stage.usd,
                cached: c.stage.cached,
                attempts: c.stage.attempts,
            });
            ledger.push(Entry {
                kind: Kind::Rollout,
                label: c.stage.label.clone(),
                usd: if c.rollout.cached {
                    0.0
                } else {
                    c.rollout.outcome.usd
                },
                cached: c.rollout.cached,
                attempts: 1,
            });
        }
        self.update(|summary| {
            let stages = &mut summary.mode(mode).stages;
            for c in steps {
                stages.retain(|s| s.label != c.stage.label);
                stages.push(StageSummary {
                    label: c.stage.label.clone(),
                    fell_back: c.stage.fell_back,
                    attempts: c.stage.attempts,
                    kept_base: c.stage.kept_base,
                    usd: c.stage.usd,
                    cached: c.stage.cached,
                    error: c.stage.error.clone(),
                    rollout_ok: c.rollout.outcome.ok,
                });
            }
        });
    }

    fn halted(&self, run: &mut ModeRun) -> bool {
        run.stopped = self.gate.stopped();
        run.stopped.is_some()
    }

    /// Parallel mode: 2k stages with identical evidence, then k
    /// independent compositions over the pool of all of them.
    pub(crate) async fn parallel(
        &self,
        plan: &ModePlan,
        base: &[RolloutDone],
        shared: bool,
    ) -> ModeRun {
        let mut run = ModeRun {
            ledger: ModeLedger::new(plan.mode),
            ..ModeRun::default()
        };
        Self::base_entries(&mut run.ledger, base, shared);
        let rollouts = base_view(base);
        let stages: Vec<&Step> = plan.steps.iter().filter(|s| !s.is_final()).collect();
        let candidates = join_all(stages.iter().map(|s| {
            self.step(StageSpec {
                label: &s.label,
                kind: s.kind,
                mode: plan.mode,
                fallback: &self.active,
                harness: &self.active,
                rollouts: &rollouts,
                pool: &[],
                decisions: false,
            })
        }))
        .await;
        self.record(plan.mode, &mut run.ledger, &candidates);
        if self.halted(&mut run) {
            return run;
        }
        let pool = pool_view(&candidates);
        let finals: Vec<&Step> = plan.steps.iter().filter(|s| s.is_final()).collect();
        let composed = join_all(finals.iter().map(|s| {
            self.step(StageSpec {
                label: &s.label,
                kind: StepKind::Composition,
                mode: plan.mode,
                fallback: &self.active,
                harness: &self.active,
                rollouts: &rollouts,
                pool: &pool,
                decisions: true,
            })
        }))
        .await;
        self.record(plan.mode, &mut run.ledger, &composed);
        let trees: Vec<&Tree> = composed.iter().map(|c| &c.stage.tree).collect();
        run.similarity = Some(similarity(&trees));
        run.finals = composed;
        self.halted(&mut run);
        run
    }

    /// Sequential mode: k rounds, each proposing against the current
    /// checkpoint and composing the next one from the whole history.
    pub(crate) async fn sequential(
        &self,
        plan: &ModePlan,
        base: &[RolloutDone],
        shared: bool,
    ) -> ModeRun {
        let mut run = ModeRun {
            ledger: ModeLedger::new(plan.mode),
            ..ModeRun::default()
        };
        Self::base_entries(&mut run.ledger, base, shared);
        let rollouts = base_view(base);
        let mut history: Vec<Candidate> = Vec::new();
        let mut checkpoint = self.active.clone();
        for round in plan.steps.chunks(3) {
            let [inv, dom, c] = round else {
                break;
            };
            let pool = pool_view(&history);
            let proposed = join_all([inv, dom].iter().map(|s| {
                self.step(StageSpec {
                    label: &s.label,
                    kind: s.kind,
                    mode: plan.mode,
                    fallback: &checkpoint,
                    harness: &checkpoint,
                    rollouts: &rollouts,
                    pool: &pool,
                    decisions: !pool.is_empty(),
                })
            }))
            .await;
            self.record(plan.mode, &mut run.ledger, &proposed);
            history.extend(proposed);
            if self.halted(&mut run) {
                return run;
            }
            let pool = pool_view(&history);
            let composed = self
                .step(StageSpec {
                    label: &c.label,
                    kind: StepKind::Checkpoint,
                    mode: plan.mode,
                    fallback: &checkpoint,
                    harness: &checkpoint,
                    rollouts: &rollouts,
                    pool: &pool,
                    decisions: true,
                })
                .await;
            self.record(plan.mode, &mut run.ledger, std::slice::from_ref(&composed));
            checkpoint = composed.stage.tree.clone();
            run.finals.push(composed.clone());
            history.push(composed);
            if self.halted(&mut run) {
                return run;
            }
        }
        run
    }
}

/// `{n, n_distinct, mean_line_jaccard}` over `trees`.
#[must_use]
pub fn similarity(trees: &[&Tree]) -> Similarity {
    let lines: Vec<BTreeSet<String>> = trees
        .iter()
        .map(|t| {
            t.files
                .iter()
                .flat_map(|(path, text)| text.lines().map(move |l| format!("{path}\0{l}")))
                .collect()
        })
        .collect();
    let distinct: BTreeSet<String> = trees.iter().map(|t| t.fingerprint()).collect();
    let mut sum = 0.0;
    let mut pairs = 0_u32;
    for (i, a) in lines.iter().enumerate() {
        for b in &lines[i + 1..] {
            let union = a.union(b).count();
            let shared = a.intersection(b).count();
            sum += if union == 0 {
                1.0
            } else {
                f64::from(u32::try_from(shared).unwrap_or(u32::MAX))
                    / f64::from(u32::try_from(union).unwrap_or(u32::MAX))
            };
            pairs += 1;
        }
    }
    Similarity {
        n: trees.len(),
        n_distinct: distinct.len(),
        mean_line_jaccard: if pairs == 0 {
            1.0
        } else {
            sum / f64::from(pairs)
        },
    }
}
