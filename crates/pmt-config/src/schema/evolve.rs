//! The `[evolve]` and `[harness]` sections (spec: harness-evolution).
//! Evolution is opt-in per profile; the harness keys apply whether or not
//! it is on, because users edit and roll back harnesses by hand too.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[evolve]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct EvolveConfig {
    /// Allow harness evolution in this profile (`prometheus evolve`,
    /// `/evolve`, the `evolve.*` RPC methods, scheduled and automatic
    /// jobs).
    pub enabled: bool,
    /// Recursion budget k: each mode spends 4k rollouts and 3k improver
    /// sessions.
    #[schemars(range(min = 1, max = 8))]
    pub k: u8,
    /// `parallel` (width), `sequential` (depth) or `both` (one proposal
    /// per mode on shared base rollouts).
    pub mode: EvolveMode,
    /// Files never captured into a task, evidence or library entry,
    /// matched against the full relative path (leading dots included).
    pub leak_globs: Vec<String>,
    /// Automatic jobs after failed or long sessions.
    pub auto: EvolveAutoConfig,
    /// Task capture.
    pub task: EvolveTaskConfig,
    /// How rollouts run.
    pub rollout: EvolveRolloutConfig,
    /// How improver sessions run.
    pub improver: EvolveImproverConfig,
    /// What improvers see of the rollouts.
    pub evidence: EvolveEvidenceConfig,
    /// The spend cap of one job.
    pub budget: EvolveBudgetConfig,
    /// Concurrent work across every job of one process.
    pub concurrency: EvolveConcurrencyConfig,
    /// What happens to a proposal.
    pub adoption: EvolveAdoptionConfig,
    /// Who picks the final of each mode: `model` (a blind selector
    /// session) or `user` (`prometheus evolve choose`).
    pub selection: EvolveSelection,
    /// Job directories older than this are pruned (the summary of a job
    /// whose proposal was adopted is kept).
    #[schemars(range(min = 1))]
    pub retention_secs: u64,
    /// Interrupted jobs are queued again when the daemon starts.
    pub resume_on_start: bool,
}

/// Default leak globs: grader material never reaches an evolution job.
pub const DEFAULT_LEAK_GLOBS: &[&str] = &[
    "**/*reference_answer*",
    "**/*rubric*",
    "**/expected/**",
    "**/*.expected.*",
    "**/.pmt-eval/**",
];

impl Default for EvolveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            k: 2,
            mode: EvolveMode::Parallel,
            leak_globs: DEFAULT_LEAK_GLOBS.iter().map(|g| (*g).to_owned()).collect(),
            auto: EvolveAutoConfig::default(),
            task: EvolveTaskConfig::default(),
            rollout: EvolveRolloutConfig::default(),
            improver: EvolveImproverConfig::default(),
            evidence: EvolveEvidenceConfig::default(),
            budget: EvolveBudgetConfig::default(),
            concurrency: EvolveConcurrencyConfig::default(),
            adoption: EvolveAdoptionConfig::default(),
            selection: EvolveSelection::Model,
            retention_secs: 2_592_000,
            resume_on_start: true,
        }
    }
}

/// Who selects a mode's final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvolveSelection {
    /// A blind, anonymised selector session.
    Model,
    /// The user, after comparing the anonymised candidates.
    User,
}

/// `[evolve.rollout]`: the headless sessions that try a harness on the
/// task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveRolloutConfig {
    /// Execution backend: `local-sandbox` (the `local` backend with full
    /// sandbox enforcement) or another sandboxed backend (`container`,
    /// `ssh`, `singularity`, a plugin backend). The unsandboxed host is
    /// refused.
    pub backend: String,
    /// `providers`: egress only to the configured model providers;
    /// `session`: the source session's egress rules.
    pub network: RolloutNetwork,
    /// Model calls one rollout may make.
    #[schemars(range(min = 1))]
    pub max_iterations: u32,
    /// Wall-clock limit of one rollout.
    #[schemars(range(min = 1))]
    pub timeout_secs: u64,
}

impl Default for EvolveRolloutConfig {
    fn default() -> Self {
        Self {
            backend: "local-sandbox".to_owned(),
            network: RolloutNetwork::Providers,
            max_iterations: 60,
            timeout_secs: 900,
        }
    }
}

/// A rollout's network egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RolloutNetwork {
    /// Only the configured model providers.
    Providers,
    /// The source session's rules.
    Session,
}

/// `[evolve.improver]`: the propose, compose, select and reflect
/// sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveImproverConfig {
    /// `provider/model` of every improver session; unset uses the
    /// `evolution` auxiliary route, else the profile's main model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Wall-clock limit of one session.
    #[schemars(range(min = 1))]
    pub timeout_secs: u64,
    /// Model calls one session may make.
    #[schemars(range(min = 1))]
    pub max_iterations: u32,
}

impl Default for EvolveImproverConfig {
    fn default() -> Self {
        Self {
            model: None,
            timeout_secs: 1800,
            max_iterations: 120,
        }
    }
}

/// `[evolve.evidence]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveEvidenceConfig {
    /// A tool observation longer than this is truncated in the rollout
    /// trajectories improvers read.
    #[schemars(range(min = 10000))]
    pub obs_trunc_chars: usize,
}

impl Default for EvolveEvidenceConfig {
    fn default() -> Self {
        Self {
            obs_trunc_chars: 50_000,
        }
    }
}

/// `[evolve.budget]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveBudgetConfig {
    /// A job stops (`budget-exceeded`) once its spend reaches this many
    /// US dollars, unless `--budget-usd` names another cap.
    #[schemars(range(min = 0.0))]
    pub max_usd: f64,
}

impl Default for EvolveBudgetConfig {
    fn default() -> Self {
        Self { max_usd: 5.0 }
    }
}

/// `[evolve.concurrency]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveConcurrencyConfig {
    /// Improver sessions running at once.
    #[schemars(range(min = 1))]
    pub sessions: u32,
    /// Rollouts running at once.
    #[schemars(range(min = 1))]
    pub rollouts: u32,
}

impl Default for EvolveConcurrencyConfig {
    fn default() -> Self {
        Self {
            sessions: 2,
            rollouts: 2,
        }
    }
}

/// `[evolve.adoption]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveAdoptionConfig {
    /// `ask` (default), `auto` (adopt at once, except proposals touching
    /// scripts, hooks or subagents, which always wait for approval) or
    /// `never` (proposals are kept for review).
    pub policy: AdoptionPolicy,
}

impl Default for EvolveAdoptionConfig {
    fn default() -> Self {
        Self {
            policy: AdoptionPolicy::Ask,
        }
    }
}

/// What happens to a proposal once a job produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionPolicy {
    /// The user adopts or rejects it.
    Ask,
    /// Adopted at once unless it touches scripts, hooks or subagents.
    Auto,
    /// Kept for review only.
    Never,
}

/// How an evolution job searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvolveMode {
    /// k independent compositions of 2k candidates.
    Parallel,
    /// k rounds, each composing a checkpoint from the history.
    Sequential,
    /// Both modes on the same base rollouts; at most one is adopted.
    Both,
}

impl EvolveMode {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Sequential => "sequential",
            Self::Both => "both",
        }
    }

    /// The mode spelled `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Parallel, Self::Sequential, Self::Both]
            .into_iter()
            .find(|m| m.as_str() == name)
    }
}

/// `[evolve.auto]`: jobs queued when a session ends badly or long.
#[allow(clippy::struct_excessive_bools)] // independent switches of one section
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveAutoConfig {
    /// Queue jobs automatically at all.
    pub enabled: bool,
    /// After a session that ended in error or interruption after at
    /// least one tool call.
    pub on_failure: bool,
    /// After a session of at least `min_iterations` iterations.
    pub on_long_task: bool,
    /// What counts as a long session.
    #[schemars(range(min = 1))]
    pub min_iterations: u32,
    /// Automatic jobs per profile per day.
    pub max_jobs_per_day: u32,
}

impl Default for EvolveAutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_failure: true,
            on_long_task: true,
            min_iterations: 40,
            max_jobs_per_day: 3,
        }
    }
}

/// `[evolve.task]`: freezing a job's task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvolveTaskConfig {
    /// Largest workspace snapshot of a task, in bytes.
    #[schemars(range(min = 1))]
    pub max_source_bytes: u64,
}

impl Default for EvolveTaskConfig {
    fn default() -> Self {
        Self {
            max_source_bytes: 52_428_800,
        }
    }
}

/// The `[harness]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct HarnessConfig {
    /// Combined characters of `SYSTEM.md`, `GUARDRAILS.md`,
    /// `MEMORY_POLICY.md` and `TOOL_GUIDANCE.md` a harness may hold.
    #[schemars(range(min = 4000, max = 64000))]
    pub always_on_cap_chars: u32,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            always_on_cap_chars: 24_000,
        }
    }
}
