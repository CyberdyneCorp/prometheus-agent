//! Contracts of harness evolution (spec: harness-evolution, Job
//! lifecycle, events and RPC): the `evolve.*` and `harness.*` methods the
//! dashboard and the desktop use. Each does what the matching
//! `prometheus evolve` or `prometheus harness` subcommand does; a job
//! started over RPC is run by the daemon's evolution runner.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// Params of every `evolve.*` and `harness.*` method; each uses the
/// fields its command takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvolveParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// `evolve.start`: the session whose task to evolve for.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `evolve.start`: a task file on the daemon's machine.
    #[serde(default)]
    pub task_file: Option<String>,
    /// `evolve.start`: the recursion budget (1-8).
    #[serde(default)]
    pub k: Option<u8>,
    /// `evolve.start`: `parallel`, `sequential` or `both`.
    #[serde(default)]
    pub mode: Option<String>,
    /// `evolve.start`: the experience-library domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// `evolve.start`: the spend cap in US dollars.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// The job (`evolve.status`, `evolve.cancel`, `evolve.choose`).
    #[serde(default)]
    pub job_id: Option<String>,
    /// `evolve.choose`: the anonymous candidate id.
    #[serde(default)]
    pub candidate: Option<String>,
    /// The proposal (`evolve.proposals.adopt`, `evolve.proposals.reject`).
    #[serde(default)]
    pub proposal_id: Option<String>,
    /// `evolve.proposals.adopt`, `harness.rollback`: also rebuild the
    /// prompt of open sessions at their next turn.
    #[serde(default)]
    pub now: bool,
    /// `evolve.proposals.adopt`: rebase onto a changed active harness.
    #[serde(default)]
    pub rebase: bool,
    /// `harness.rollback`: the version to roll back to; `harness.diff`:
    /// the first version.
    #[serde(default)]
    pub version: Option<String>,
    /// `harness.diff`: the second version (default: the active one).
    #[serde(default)]
    pub other: Option<String>,
}

/// Result of the methods that act on one job, proposal or version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvolveReply {
    /// What happened, in one line.
    pub message: String,
    /// The matching command's `--json` document, or `null`.
    #[serde(default)]
    pub data: Value,
}

/// One job of `evolve.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvolveJobRow {
    /// `evo_<ULID>`.
    pub id: String,
    /// The job status.
    pub status: String,
    /// What queued it.
    pub trigger: String,
    /// `parallel`, `sequential` or `both`.
    pub mode: String,
    /// The recursion budget.
    pub k: u8,
    /// The spend cap in US dollars (`null`: `evolve.budget.max_usd`).
    pub budget: Option<f64>,
    /// Rollouts the job plans.
    pub rollouts: usize,
    /// Spend so far.
    pub cost_usd: f64,
    /// The experience-library domain.
    pub domain: String,
    /// RFC 3339.
    pub created_at: String,
}

/// `evolve.list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvolveJobs {
    /// Oldest first.
    pub jobs: Vec<EvolveJobRow>,
}

/// `evolve.proposals.list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvolveProposals {
    /// Each proposal's `<id>.toml` as JSON, oldest first.
    pub proposals: Vec<Value>,
}

/// `harness.log` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessVersions {
    /// The active version.
    pub active: String,
    /// Each version's `version.toml` as JSON, newest first.
    pub versions: Vec<Value>,
}

method!(/// `evolve.start`: queues a job (the daemon runs it).
    EvolveStart = "evolve.start" (EvolveParams) -> EvolveReply);
method!(/// `evolve.status`: a job's `summary.json`.
    EvolveStatus = "evolve.status" (EvolveParams) -> EvolveReply);
method!(/// `evolve.list`: the profile's jobs.
    EvolveList = "evolve.list" (EvolveParams) -> EvolveJobs);
method!(/// `evolve.cancel`: stops a job at its next model-call boundary.
    EvolveCancel = "evolve.cancel" (EvolveParams) -> EvolveReply);
method!(/// `evolve.choose`: picks a candidate of a job awaiting selection.
    EvolveChoose = "evolve.choose" (EvolveParams) -> EvolveReply);
method!(/// `evolve.proposals.list`: the profile's proposals.
    EvolveProposalsList = "evolve.proposals.list" (EvolveParams) -> EvolveProposals);
method!(/// `evolve.proposals.adopt`: adopts a proposal as a new version.
    EvolveProposalsAdopt = "evolve.proposals.adopt" (EvolveParams) -> EvolveReply);
method!(/// `evolve.proposals.reject`: rejects a pending proposal.
    EvolveProposalsReject = "evolve.proposals.reject" (EvolveParams) -> EvolveReply);
method!(/// `harness.log`: the harness versions.
    HarnessLog = "harness.log" (EvolveParams) -> HarnessVersions);
method!(/// `harness.diff`: the summary and unified diff of two versions.
    HarnessDiff = "harness.diff" (EvolveParams) -> EvolveReply);
method!(/// `harness.rollback`: activates a new version with an old tree.
    HarnessRollback = "harness.rollback" (EvolveParams) -> EvolveReply);
