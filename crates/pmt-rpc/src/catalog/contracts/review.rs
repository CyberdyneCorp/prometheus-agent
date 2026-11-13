//! The git review pane and the Projects sidebar (spec: desktop-app, Git
//! review pane; Repository discovery for the Projects sidebar; projects,
//! Repository discovery).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `git.review` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitReviewParams {
    /// The session whose workspace is reviewed.
    pub session_id: String,
    /// `uncommitted` (default), `branch` or `last-turn`.
    #[serde(default = "uncommitted")]
    pub scope: String,
}

fn uncommitted() -> String {
    "uncommitted".to_owned()
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitFile {
    /// Its path in the repository.
    pub path: String,
    /// `added`, `modified`, `deleted` or `type-changed`.
    pub status: String,
    /// Part of the change is staged (`uncommitted` only).
    pub staged: bool,
    /// The unified diff (cut at 256 KiB).
    pub patch: String,
}

/// `git.review` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitReview {
    /// The working tree's top folder.
    pub root: String,
    /// The checked-out branch (`None`: detached).
    pub branch: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// The scope shown.
    pub scope: String,
    /// The changed files.
    pub files: Vec<GitFile>,
    /// Nothing in this scope may be staged, reverted or committed.
    pub read_only: bool,
    /// The `gh` executable is available (Create PR is offered).
    pub gh: bool,
}

/// `git.stage`, `git.unstage` and `git.revert` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitPaths {
    /// The session.
    pub session_id: String,
    /// Paths in the repository.
    pub paths: Vec<String>,
}

/// `git.commit` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitCommitParams {
    /// The session.
    pub session_id: String,
    /// The commit message.
    pub message: String,
    /// Commit & Push: commit and push through the session's terminal
    /// backend.
    #[serde(default)]
    pub push: bool,
}

/// `git.branch` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchParams {
    /// The session.
    pub session_id: String,
    /// The branch.
    pub name: String,
    /// Create it (`git switch -c`) instead of switching to it.
    #[serde(default)]
    pub create: bool,
}

/// What a command run through the session's terminal backend printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRan {
    /// The commit made (`git.commit`).
    #[serde(default)]
    pub commit: Option<String>,
    /// Its output.
    pub output: String,
    /// Whether it succeeded.
    pub ok: bool,
}

/// `git.message` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitMessage {
    /// The suggested commit message (empty when no auxiliary route
    /// answers).
    pub message: String,
}

method!(/// `git.review`: the session's repository in one scope.
    GitReviewCall = "git.review" (GitReviewParams) -> GitReview);
method!(/// `git.stage`: stages uncommitted changes (drivers only).
    GitStage = "git.stage" (GitPaths) -> super::Ack);
method!(/// `git.unstage`: unstages changes (drivers only).
    GitUnstage = "git.unstage" (GitPaths) -> super::Ack);
method!(/// `git.revert`: discards uncommitted changes (drivers only; the
    /// pane asks first).
    GitRevert = "git.revert" (GitPaths) -> super::Ack);
method!(/// `git.commit`: commits what is staged, and with `push` pushes
    /// through the session's terminal backend (drivers only).
    GitCommit = "git.commit" (GitCommitParams) -> GitRan);
method!(/// `git.branch`: creates or switches a branch through the session's
    /// terminal backend (drivers only).
    GitBranch = "git.branch" (GitBranchParams) -> GitRan);
method!(/// `git.message`: a commit message for the staged diff from the
    /// `commit_message` auxiliary route.
    GitMessageCall = "git.message" (super::SessionRef) -> GitMessage);
method!(/// `git.pr`: `gh pr create --fill` through the session's terminal
    /// backend (drivers only).
    GitPr = "git.pr" (super::SessionRef) -> GitRan);

/// `projects.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsListParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// One explicit project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectRow {
    /// `prj_...`.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// Its slug.
    pub slug: String,
    /// Its primary folder.
    pub path: Option<String>,
}

/// A repository offered for project creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoRow {
    /// Its folder.
    pub path: String,
    /// Its folder name.
    pub name: String,
    /// When a scan last saw it (discovered rows; RFC 3339).
    #[serde(default)]
    pub last_seen: Option<String>,
}

/// `projects.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsList {
    /// Projects created explicitly.
    pub projects: Vec<ProjectRow>,
    /// Repositories of the profile's sessions that are no project's.
    pub inferred: Vec<RepoRow>,
    /// Repositories the background scan found that are no project's.
    pub discovered: Vec<RepoRow>,
    /// A scan is running or this listing started one: list again for
    /// its rows.
    pub scanning: bool,
}

/// `projects.create` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsCreateParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The repository folder.
    pub path: String,
    /// The display name (the folder name when omitted).
    #[serde(default)]
    pub name: Option<String>,
}

method!(/// `projects.list`: explicit projects, repositories inferred from
    /// sessions and discovered ones (the desktop sees discovery unless
    /// `projects.repo_scan_enabled = false`).
    ProjectsListCall = "projects.list" (ProjectsListParams) -> ProjectsList);
method!(/// `projects.create`: a project of one repository folder.
    ProjectsCreate = "projects.create" (ProjectsCreateParams) -> ProjectRow);
