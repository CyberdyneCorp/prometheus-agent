//! The `[evolve]` and `[harness]` sections (spec: harness-evolution).
//! Evolution is opt-in per profile; the harness keys apply whether or not
//! it is on, because users edit and roll back harnesses by hand too.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[evolve]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
        }
    }
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
