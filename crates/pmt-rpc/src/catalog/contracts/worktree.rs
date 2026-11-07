//! `worktree.new`, `worktree.status`, `worktree.list` and `worktree.prune`
//! (spec: git-worktrees, In-session worktree command): what `/worktree`
//! does, for a session a client drives.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// Params naming the session (`worktree.status`, `worktree.list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeSessionParams {
    /// The session (its working directory's repository).
    pub session_id: String,
}

/// `worktree.new` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeNewParams {
    /// The session to move into the new tree.
    pub session_id: String,
    /// The tree's name (`[A-Za-z0-9._-]{1,64}`); omitted: `prom-<8 hex>`.
    #[serde(default)]
    pub name: Option<String>,
}

/// One Prometheus tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeInfo {
    /// Its name (directory under `.worktrees/`).
    pub name: String,
    /// Its directory.
    pub path: String,
    /// The branch it is on (`null`: detached).
    pub branch: Option<String>,
    /// `generated`, `named`, `subagent` or `kanban`.
    pub kind: String,
    /// Seconds since it was created.
    pub age_secs: u64,
    /// The last sweep decision and its reason.
    pub last_decision: Option<String>,
}

/// `worktree.new` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeCreated {
    /// The new tree, now the session's working directory.
    pub tree: WorktreeInfo,
    /// The ref it was branched from.
    pub base: String,
    /// What the user should know (a failed fetch).
    pub notices: Vec<String>,
}

/// `worktree.status` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeStatus {
    /// The session's working directory.
    pub cwd: String,
    /// The Prometheus tree it is in (`null`: none).
    pub tree: Option<WorktreeInfo>,
}

/// One archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeArchiveInfo {
    /// The archived tree's name (`worktrees restore <name>`).
    pub name: String,
    /// `refs/prometheus/archive/<name>`.
    pub reference: String,
    /// The branch it was on.
    pub branch: Option<String>,
    /// Seconds since it was archived.
    pub age_secs: u64,
    /// Why it was archived (`sweep`, `exit`, `subagent`).
    pub reason: String,
}

/// `worktree.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeListing {
    /// The repository's main checkout.
    pub repo: String,
    /// Its Prometheus trees.
    pub trees: Vec<WorktreeInfo>,
    /// Its archives.
    pub archives: Vec<WorktreeArchiveInfo>,
}

/// `worktree.prune` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreePruneParams {
    /// The session (its working directory's repository).
    pub session_id: String,
    /// Report the decisions; change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// One sweep decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeDecision {
    /// The tree.
    pub name: String,
    /// Its directory.
    pub path: String,
    /// `remove`, `archive` or `keep`.
    pub action: String,
    /// The reason a tree is kept (`live-process`, `keep-large-untracked`...).
    pub reason: Option<String>,
    /// What happened (the plan on a dry run).
    pub outcome: String,
}

/// `worktree.prune` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreePruned {
    /// The repository's main checkout.
    pub repo: String,
    /// Nothing was changed.
    pub dry_run: bool,
    /// One decision per tree.
    pub decisions: Vec<WorktreeDecision>,
}

method!(/// `worktree.new`: creates a tree and moves the session into it.
    WorktreeNew = "worktree.new" (WorktreeNewParams) -> WorktreeCreated);
method!(/// `worktree.status`: the session's tree and branch.
    WorktreeStatusCall = "worktree.status" (WorktreeSessionParams) -> WorktreeStatus);
method!(/// `worktree.list`: the Prometheus trees and archives of the session's repository.
    WorktreeList = "worktree.list" (WorktreeSessionParams) -> WorktreeListing);
method!(/// `worktree.prune`: runs the sweep on the session's repository.
    WorktreePrune = "worktree.prune" (WorktreePruneParams) -> WorktreePruned);
