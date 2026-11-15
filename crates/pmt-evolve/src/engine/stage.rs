//! One proposal stage, composition or checkpoint (spec: harness-evolution,
//! Stage acceptance, retry and fallback; Improver sessions). The edited
//! `context/harness/` is accepted only after the auto-trigger repair,
//! validation and the leak tripwire pass. A rejection is retried once,
//! from the model's own rejected edit, with the errors appended to the
//! mission; two rejections, a failed evidence build or a failed session
//! (a transport failure already had its retry) make the stage inherit
//! its fallback tree.

use pmt_config::harness::Tree;

use super::evidence::{self, PoolItem, Spec};
use super::gate::Stop;
use super::ports::SessionKind;
use super::prompts::{self, IMPROVER_SYSTEM, MissionSpec};
use super::run::{Run, SessionCall, SessionError};
use crate::harness::{diff, validate};
use crate::job::budget::StepKind;

/// The longest error a stage keeps.
pub const MAX_ERROR_CHARS: usize = 400;

/// What a stage starts from.
pub(crate) struct StageSpec<'a> {
    pub label: &'a str,
    pub kind: StepKind,
    pub mode: &'a str,
    /// What the stage inherits when it fails.
    pub fallback: &'a Tree,
    /// The tree it edits.
    pub harness: &'a Tree,
    pub rollouts: &'a [evidence::Rollout<'a>],
    pub pool: &'a [PoolItem<'a>],
    pub decisions: bool,
}

/// How a stage ended.
#[derive(Debug, Clone)]
pub(crate) struct StageResult {
    pub label: String,
    pub tree: Tree,
    pub fell_back: bool,
    pub attempts: u32,
    pub usd: f64,
    pub cached: bool,
    pub error: Option<String>,
    pub kept_base: bool,
    /// The decision notes: the improver's last reply.
    pub decision: String,
    pub stopped: Option<Stop>,
}

enum Attempt {
    Accepted(Tree),
    Rejected(Vec<String>, Tree),
    Fatal(String),
    Stopped(Stop),
}

impl Run<'_> {
    /// Runs one stage.
    pub(crate) async fn stage(&self, spec: &StageSpec<'_>) -> StageResult {
        let mut result = StageResult {
            label: spec.label.to_owned(),
            tree: spec.fallback.clone(),
            fell_back: true,
            attempts: 0,
            usd: 0.0,
            cached: true,
            error: None,
            kept_base: true,
            decision: String::new(),
            stopped: None,
        };
        let mut harness = spec.harness.clone();
        let mut errors: Vec<String> = Vec::new();
        for attempt in 1..=2 {
            result.attempts = attempt;
            match self.attempt(spec, &harness, &errors, &mut result).await {
                Attempt::Accepted(tree) => {
                    result.kept_base = diff::summary(spec.fallback, &tree).is_empty();
                    result.tree = tree;
                    result.fell_back = false;
                    self.log.stage(spec.label, "accepted");
                    return result;
                }
                Attempt::Rejected(why, edited) => {
                    errors = why;
                    harness = edited;
                }
                Attempt::Fatal(why) => {
                    errors = vec![why];
                    break;
                }
                Attempt::Stopped(stop) => {
                    result.stopped = Some(stop);
                    errors = vec![stop.status().to_owned()];
                    break;
                }
            }
        }
        result.error = Some(truncate(&errors.join("; "), MAX_ERROR_CHARS));
        self.log.stage(spec.label, "fell_back");
        result
    }

    async fn attempt(
        &self,
        spec: &StageSpec<'_>,
        harness: &Tree,
        errors: &[String],
        result: &mut StageResult,
    ) -> Attempt {
        let name = format!("{}-{}-{}", spec.mode, spec.label, result.attempts);
        let workspace = match self.scratch(&name) {
            Ok(dir) => dir,
            Err(err) => return Attempt::Fatal(format!("evidence: {err}")),
        };
        let outcome = self
            .attempt_in(spec, harness, errors, result, &workspace)
            .await;
        let _ = std::fs::remove_dir_all(&workspace);
        outcome
    }

    async fn attempt_in(
        &self,
        spec: &StageSpec<'_>,
        harness: &Tree,
        errors: &[String],
        result: &mut StageResult,
        workspace: &std::path::Path,
    ) -> Attempt {
        let library = self.engine.library.as_deref();
        let layout = match evidence::build(
            workspace,
            &Spec {
                task: &self.dir.join("task"),
                rollouts: spec.rollouts,
                harness,
                pool: spec.pool,
                decisions: spec.decisions,
                library,
                obs_trunc: self.engine.config.evidence.obs_trunc_chars.max(10_000),
            },
        ) {
            Ok(layout) => layout,
            Err(err) => return Attempt::Fatal(err),
        };
        let context = evidence::context(workspace);
        if let Err(hit) = self.tripwire.check_dir(&context) {
            tracing::warn!(job = %self.job.id, stage = spec.label, "{hit}");
            return Attempt::Fatal(hit);
        }
        let mut mission = prompts::mission(&MissionSpec {
            label: spec.label,
            kind: spec.kind,
            mode: spec.mode,
            domain: &self.job.domain,
            layout: &layout,
        });
        if !errors.is_empty() {
            mission = prompts::rejected(&mission, errors);
        }
        if let Err(err) = std::fs::write(context.join("MISSION.md"), &mission) {
            return Attempt::Fatal(format!("evidence: {err}"));
        }
        let kind = if matches!(spec.kind, StepKind::Composition | StepKind::Checkpoint) {
            SessionKind::Compose
        } else {
            SessionKind::Propose
        };
        let call = SessionCall {
            label: spec.label,
            kind,
            system: IMPROVER_SYSTEM,
            mission: &mission,
            workspace,
        };
        match self.session(call).await {
            Ok(done) => {
                result.usd += done.usd;
                result.cached &= done.cached;
                result.decision.clone_from(&done.text);
                self.accept(&context.join("harness"), &done.text, spec.label)
            }
            Err(SessionError::Stopped(stop)) => Attempt::Stopped(stop),
            Err(SessionError::Failed { message, usd }) => {
                result.usd += usd;
                result.cached = false;
                Attempt::Fatal(message)
            }
        }
    }

    /// Accepts the edited tree at `dir` or lists why not.
    fn accept(&self, dir: &std::path::Path, reply: &str, label: &str) -> Attempt {
        let (mut tree, problems) = Tree::read(dir).unwrap_or_default();
        let edited = tree.clone();
        let mut errors = problems;
        if reply.trim().is_empty() {
            errors.push("no reply".to_owned());
        }
        if tree.files.is_empty() {
            errors.push("the harness is empty".to_owned());
        }
        if errors.is_empty() {
            match validate::validate(&mut tree, self.engine.rules) {
                Ok(_) => {}
                Err(found) => errors.extend(found),
            }
        }
        if errors.is_empty()
            && let Err(hit) = self.tripwire.check_tree(&tree)
        {
            tracing::warn!(job = %self.job.id, stage = label, "{hit}");
            errors.push(hit);
        }
        if errors.is_empty() {
            Attempt::Accepted(tree)
        } else {
            Attempt::Rejected(errors, edited)
        }
    }
}

/// `text` cut to `max` characters.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect()
}
