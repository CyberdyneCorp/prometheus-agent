//! The board's vocabulary: task statuses, actors, block kinds, workspace
//! and kind specs, and the rows read back from `board.db`.

use std::fmt;
use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// A task's status (spec: kanban, Typed workflow table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// An idea waiting to be specified.
    Triage,
    /// Waiting for its parents.
    Todo,
    /// Parked until `wake_at` (or an unblock).
    Scheduled,
    /// Claimable by the dispatcher.
    Ready,
    /// Claimed: a worker owns it.
    Running,
    /// Stopped by a typed block.
    Blocked,
    /// Waiting for a reviewer.
    Review,
    /// Finished.
    Done,
    /// Put away; restorable to `triage`, `todo` or `ready`.
    Archived,
}

impl Status {
    /// Every status, in board order.
    pub const ALL: [Self; 9] = [
        Self::Triage,
        Self::Todo,
        Self::Scheduled,
        Self::Ready,
        Self::Running,
        Self::Blocked,
        Self::Review,
        Self::Done,
        Self::Archived,
    ];

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Todo => "todo",
            Self::Scheduled => "scheduled",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }

    /// Parses a stored or typed status.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` naming the accepted values.
    pub fn parse(text: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .ok_or_else(|| {
                let all: Vec<_> = Self::ALL.iter().map(|s| s.as_str()).collect();
                invalid(format!(
                    "unknown status '{text}' (expected {})",
                    all.join("|")
                ))
            })
    }

    /// Whether a parent in this status no longer gates its children.
    #[must_use]
    pub const fn settles_parent(self) -> bool {
        matches!(self, Self::Done | Self::Archived)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who asks for a transition; the workflow table allows each transition
/// to named actors only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// A person: the CLI, the dashboard, the desktop app.
    User,
    /// The worker session of the task's open run.
    Worker,
    /// A reviewer (person or reviewer profile) acting on a `review` task.
    Reviewer,
    /// The dispatcher: promotion, claims, reclaims, wake-ups.
    Dispatcher,
}

impl ActorKind {
    /// Every actor kind.
    pub const ALL: [Self; 4] = [Self::User, Self::Worker, Self::Reviewer, Self::Dispatcher];

    /// The spelling in messages and events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Dispatcher => "dispatcher",
        }
    }
}

impl fmt::Display for ActorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An actor: its kind, a display name (profile, user) and, for a worker,
/// the run it acts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// What the workflow table checks.
    pub kind: ActorKind,
    /// Who, for the event log (`writer`, `cli`).
    pub name: String,
    /// The worker's run.
    pub run: Option<String>,
}

impl Actor {
    /// A person at a surface (`cli`, `dashboard`).
    #[must_use]
    pub fn user(name: &str) -> Self {
        Self {
            kind: ActorKind::User,
            name: name.to_owned(),
            run: None,
        }
    }

    /// The worker of `run`, running as `profile`.
    #[must_use]
    pub fn worker(profile: &str, run: &str) -> Self {
        Self {
            kind: ActorKind::Worker,
            name: profile.to_owned(),
            run: Some(run.to_owned()),
        }
    }

    /// A reviewer.
    #[must_use]
    pub fn reviewer(name: &str) -> Self {
        Self {
            kind: ActorKind::Reviewer,
            name: name.to_owned(),
            run: None,
        }
    }

    /// The dispatcher.
    #[must_use]
    pub fn dispatcher() -> Self {
        Self {
            kind: ActorKind::Dispatcher,
            name: "dispatcher".to_owned(),
            run: None,
        }
    }

    /// `kind:name`, stored in events.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }
}

/// Why a task is blocked (spec: kanban, Typed blocks and block-loop
/// breaker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Waits for another task: routes to `todo`.
    Dependency,
    /// Needs a person's answer.
    NeedsInput,
    /// Lacks a capability (profile, credential, skill).
    Capability,
    /// Its cost budget is spent.
    Budget,
    /// A passing problem.
    Transient,
}

impl BlockKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Dependency,
        Self::NeedsInput,
        Self::Capability,
        Self::Budget,
        Self::Transient,
    ];

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dependency => "dependency",
            Self::NeedsInput => "needs_input",
            Self::Capability => "capability",
            Self::Budget => "budget",
            Self::Transient => "transient",
        }
    }

    /// Parses a kind.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` naming the accepted kinds.
    pub fn parse(text: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == text)
            .ok_or_else(|| {
                let all: Vec<_> = Self::ALL.iter().map(|k| k.as_str()).collect();
                invalid(format!(
                    "unknown block kind '{text}' (expected {})",
                    all.join("|")
                ))
            })
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a task's worker works (spec: kanban, Workspaces).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Workspace {
    /// `<board>/workspaces/<task>/`, deleted after completion.
    Scratch,
    /// A git worktree on `kanban/<task>` or the given branch, preserved.
    Worktree {
        /// The branch; `None` uses `kanban/<task_id>`.
        branch: Option<String>,
    },
    /// An existing absolute directory, preserved.
    Dir {
        /// The directory.
        path: PathBuf,
    },
}

impl Workspace {
    /// Parses `scratch`, `worktree[:<branch>]` or `dir:<absolute path>`.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        if text == "scratch" {
            return Ok(Self::Scratch);
        }
        if text == "worktree" {
            return Ok(Self::Worktree { branch: None });
        }
        if let Some(branch) = text.strip_prefix("worktree:") {
            let branch = branch.trim();
            if branch.is_empty() || branch.starts_with('-') || branch.contains("..") {
                return Err(invalid(format!("invalid worktree branch '{branch}'")));
            }
            return Ok(Self::Worktree {
                branch: Some(branch.to_owned()),
            });
        }
        if let Some(path) = text.strip_prefix("dir:") {
            let path = PathBuf::from(path.trim());
            if !path.is_absolute() {
                return Err(invalid("workspace dir must be an absolute path"));
            }
            return Ok(Self::Dir { path });
        }
        Err(invalid(format!(
            "invalid workspace '{text}' (expected scratch, worktree[:<branch>] or dir:<absolute path>)"
        )))
    }

    /// The typed spelling.
    #[must_use]
    pub fn spec(&self) -> String {
        match self {
            Self::Scratch => "scratch".to_owned(),
            Self::Worktree { branch: None } => "worktree".to_owned(),
            Self::Worktree { branch: Some(b) } => format!("worktree:{b}"),
            Self::Dir { path } => format!("dir:{}", path.display()),
        }
    }
}

/// How an `eval` task's model is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    /// Real provider calls.
    Live,
    /// Recorded responses only: no live request.
    Replay,
}

/// How an `evolve` task searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolveMode {
    /// Candidates side by side.
    Parallel,
    /// One candidate after another.
    Sequential,
}

/// What a task's worker does (spec: kanban, Eval and harness-evolution
/// tasks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// An agent session driving the task through the kanban tools.
    Agent,
    /// `prometheus eval run <suite>` for the assignee profile.
    Eval {
        /// The suite.
        suite: String,
        /// Live or replayed provider (default live).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_mode: Option<ProviderMode>,
    },
    /// A harness-evolution session; always finishes in `review`.
    Evolve {
        /// The task file or session the evolution targets.
        target_task: String,
        /// The search mode.
        mode: EvolveMode,
        /// Candidates per round.
        budget_k: u32,
    },
}

impl TaskKind {
    /// `agent`, `eval` or `evolve`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Eval { .. } => "eval",
            Self::Evolve { .. } => "evolve",
        }
    }
}

/// One task row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Task {
    /// `t_` + 8 lowercase hex characters.
    pub id: String,
    /// The title.
    pub title: String,
    /// The body (acceptance criteria).
    pub body: String,
    /// Its status.
    pub status: Status,
    /// The worker profile.
    pub assignee: Option<String>,
    /// The reviewer profile.
    pub reviewer: Option<String>,
    /// Higher is claimed first.
    pub priority: i64,
    /// Where the worker works.
    pub workspace: Workspace,
    /// The directory the workspace resolved to at the last claim.
    pub workspace_path: Option<PathBuf>,
    /// What the worker does.
    pub kind: TaskKind,
    /// Per-task review lane (`None`: the board's default).
    pub review: Option<bool>,
    /// The idempotency key it was created with.
    pub idempotency_key: Option<String>,
    /// The current block's kind.
    pub block_kind: Option<BlockKind>,
    /// The current block's reason.
    pub block_reason: Option<String>,
    /// Instant (Unix ms) a scheduled task wakes.
    pub wake_at: Option<i64>,
    /// Not claimed before this instant (Unix ms; rate-limit cooldown).
    pub not_before: Option<i64>,
    /// Failed runs since the last success.
    pub consecutive_failures: u32,
    /// The last failure, at most 500 characters.
    pub last_failure: Option<String>,
    /// Failures before the breaker trips (`None`: `kanban.failure_limit`).
    pub max_retries: Option<u32>,
    /// USD limit across every run.
    pub budget_usd: Option<f64>,
    /// Token limit across every run.
    pub budget_tokens: Option<u64>,
    /// Files the worker must leave in its workspace.
    pub artifacts: Vec<String>,
    /// The completion result.
    pub result: Option<String>,
    /// The completion summary.
    pub summary: Option<String>,
    /// Who created it (actor label).
    pub created_by: String,
    /// The run that created it (worker-created tasks).
    pub created_run: Option<String>,
    /// Creation instant (Unix ms).
    pub created_at: i64,
    /// Last change (Unix ms).
    pub updated_at: i64,
    /// When it reached `done` (Unix ms).
    pub completed_at: Option<i64>,
    /// `OWNER/REPO`, a pull-request URL, or `None` for `local-only`.
    pub completion_contract: Option<String>,
    /// The pull request a repository contract is bound to.
    pub pr_url: Option<String>,
    /// The worker runs in the goal loop, judged after each turn.
    pub goal_mode: bool,
    /// Judged turns before the goal is given up (`None`: 20).
    pub goal_max_turns: Option<u32>,
    /// The worker's model instead of the assignee's default.
    pub model: Option<String>,
    /// The provider of `model`.
    pub provider: Option<String>,
    /// Skills preloaded in the worker session.
    pub skills: Vec<String>,
    /// The assignee is a plugin's worker lane, not a profile.
    pub lane: bool,
    /// The project bound to the board when the task was created.
    pub project: Option<String>,
    /// The project's primary folder: the repository of its worktree.
    pub repo_root: Option<PathBuf>,
}

/// Default judged turns of a goal-mode task.
pub const GOAL_MAX_TURNS: u32 = 20;

impl Task {
    /// The branch of a `worktree` task: the given one, else
    /// `<project>/<id>[-<title-slug>]` on a board bound to a project,
    /// else `kanban/<id>`; `None` for other workspaces.
    #[must_use]
    pub fn branch(&self) -> Option<String> {
        let Workspace::Worktree { branch } = &self.workspace else {
            return None;
        };
        if let Some(branch) = branch {
            return Some(branch.clone());
        }
        Some(match &self.project {
            Some(project) => {
                let slug = title_slug(&self.title);
                if slug.is_empty() {
                    format!("{project}/{}", self.id)
                } else {
                    format!("{project}/{}-{slug}", self.id)
                }
            }
            None => format!("kanban/{}", self.id),
        })
    }

    /// The judged-turn budget of a goal-mode task.
    #[must_use]
    pub fn goal_turns(&self) -> u32 {
        self.goal_max_turns.unwrap_or(GOAL_MAX_TURNS).max(1)
    }
}

/// A title as a branch segment: lower case, every run of characters
/// outside `[a-z0-9._-]` collapsed to one `-`, trimmed, at most 40
/// characters.
#[must_use]
pub fn title_slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(40).collect();
    trimmed.trim_end_matches('-').to_owned()
}

/// One immutable event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Increasing id.
    pub id: i64,
    /// The task (`None` for board events: ticks, takeovers).
    pub task_id: Option<String>,
    /// The run it belongs to.
    pub run_id: Option<String>,
    /// Who (actor label).
    pub actor: String,
    /// What (`created`, `claimed`, `completion_refused`...).
    pub kind: String,
    /// Details.
    pub payload: serde_json::Value,
    /// When (Unix ms).
    pub at: i64,
}

/// One attempt at a task.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    /// `r_<ULID>`.
    pub id: String,
    /// Its task.
    pub task_id: String,
    /// The profile it runs as.
    pub profile: String,
    /// `worker` or `reviewer`.
    pub role: String,
    /// Claim instant (Unix ms).
    pub started_at: i64,
    /// Close instant.
    pub ended_at: Option<i64>,
    /// `completed`, `review`, `blocked`, `scheduled`, `failed`,
    /// `crashed`, `timed_out`, `rate_limited`, `spawn_failed`,
    /// `reclaimed`, `archived`.
    pub outcome: Option<String>,
    /// The worker's exit code.
    pub exit_code: Option<i32>,
    /// Its session.
    pub session_id: Option<String>,
    /// Its trajectory.
    pub trajectory_id: Option<String>,
    /// USD spent.
    pub cost_usd: f64,
    /// Tokens spent.
    pub tokens: u64,
    /// Worker pid.
    pub pid: Option<u32>,
    /// Worker process start time (Unix s).
    pub pid_start: Option<u64>,
    /// When the claim expires without a heartbeat (Unix ms).
    pub lease_expires_at: Option<i64>,
    /// Last heartbeat (Unix ms).
    pub heartbeat_at: Option<i64>,
    /// Failure text.
    pub error: Option<String>,
}

impl Run {
    /// Whether the run is still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// A comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comment {
    /// Increasing id.
    pub id: i64,
    /// Its task.
    pub task_id: String,
    /// Who wrote it.
    pub author: String,
    /// The text.
    pub body: String,
    /// When (Unix ms).
    pub at: i64,
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_INVALID_TASK, message)
}

/// The current instant in Unix ms.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Lower-case hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A fresh `<prefix>_<ulid>` id in lower case.
#[must_use]
pub fn prefixed_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        ulid::Ulid::generate().to_string().to_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_becomes_a_short_branch_segment() {
        assert_eq!(title_slug("Fix Login Bug"), "fix-login-bug");
        assert_eq!(
            title_slug("  Ünïcode / spaces!! v1.2_x "),
            "n-code-spaces-v1.2_x"
        );
        assert_eq!(title_slug(&"a".repeat(50)).len(), 40);
        assert_eq!(title_slug("***"), "");
    }
}
