//! The `[kanban]` section (spec: kanban). Boards themselves live at the
//! home root (`<home>/kanban/`), shared by every profile; these keys say
//! which board a profile works on by default and how the dispatcher that
//! runs in this profile's process claims and supervises workers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[kanban]` section.
#[allow(clippy::struct_excessive_bools)] // independent switches of one section
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct KanbanConfig {
    /// The board commands use without `--board` (unset: `default`). A
    /// board that does not exist is an error, never a silent fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_board: Option<String>,
    /// Run the dispatcher inside the gateway daemon.
    pub dispatch: bool,
    /// Seconds between dispatcher ticks.
    #[schemars(range(min = 1, max = 3600))]
    pub dispatch_interval_secs: u64,
    /// Seconds a claim lasts without a worker heartbeat before the run is
    /// reclaimed as a failure.
    #[schemars(range(min = 60, max = 86_400))]
    pub claim_ttl_secs: u64,
    /// Workers running at once across every board of the home (unset:
    /// physical memory / 512 MiB clamped to 2-8, or 2 when unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 256))]
    pub max_running: Option<u32>,
    /// Workers running at once per assignee profile.
    #[schemars(range(min = 1, max = 256))]
    pub max_running_per_profile: u32,
    /// Claim only tasks assigned to these profiles (`[]` claims nothing;
    /// unset allows any existing profile).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_profiles: Option<Vec<String>>,
    /// The assignee of ready tasks that name none (unset: such tasks are
    /// reported `skipped_unassigned`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_assignee: Option<String>,
    /// Consecutive failed runs after which a task without its own
    /// `max_retries` is blocked (`gave_up`).
    #[schemars(range(min = 1, max = 100))]
    pub failure_limit: u32,
    /// Seconds a task waits before it is claimed again after its worker
    /// was rate limited (exit 75).
    pub rate_limit_cooldown_secs: u64,
    /// Route `review` tasks to their reviewer profiles, keeping one slot
    /// for review work while a ready backlog exists.
    pub review_dispatch: bool,
    /// Largest attachment accepted on any surface.
    #[schemars(range(min = 1))]
    pub attachment_max_bytes: u64,
    /// Seconds notification subscriptions of done tasks are kept.
    pub done_sub_retention_secs: u64,
    /// `orchestrator` makes this profile's worker sessions decompose goals
    /// into linked tasks for existing profiles instead of implementing
    /// them (unset: an ordinary worker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<KanbanRole>,
    /// Let the dispatcher run `specify` on triage tasks and `decompose` on
    /// tasks assigned to an orchestrator profile before dispatching them
    /// (the dashboard's Orchestration Auto/Manual control).
    pub auto_decompose: bool,
    /// Promote decomposed children that wait for nothing straight to
    /// `ready` (else they stay in `todo`).
    pub auto_promote_children: bool,
    /// Seconds after a finished run during which its task is not spawned
    /// again unless deliberately re-queued.
    pub respawn_success_window_secs: u64,
    /// Seconds after a comment linking a GitHub pull request during which
    /// its task is not spawned again.
    pub respawn_pr_window_secs: u64,
    /// The GitHub API that PR completion contracts read (GitHub Enterprise:
    /// `https://<host>/api/v3`).
    pub github_api_url: String,
}

/// A profile's kanban role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KanbanRole {
    /// Decomposes goals into tasks for other profiles.
    Orchestrator,
}

impl Default for KanbanConfig {
    fn default() -> Self {
        Self {
            current_board: None,
            dispatch: true,
            dispatch_interval_secs: 30,
            claim_ttl_secs: 900,
            max_running: None,
            max_running_per_profile: 2,
            dispatch_profiles: None,
            default_assignee: None,
            failure_limit: 2,
            rate_limit_cooldown_secs: 300,
            review_dispatch: true,
            attachment_max_bytes: 25 * 1024 * 1024,
            done_sub_retention_secs: 2_592_000,
            role: None,
            auto_decompose: false,
            auto_promote_children: true,
            respawn_success_window_secs: 3600,
            respawn_pr_window_secs: 86_400,
            github_api_url: "https://api.github.com".to_owned(),
        }
    }
}
