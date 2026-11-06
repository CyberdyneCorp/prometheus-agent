//! The `[delegation]` and `[code_execution]` sections (spec:
//! subagent-delegation, Delegation configuration defaults; Programmatic
//! tool calling under the caller's grant; `execute_code` RPC channel and
//! limits). Both apply from the next session: a running session keeps the
//! delegation settings it started with, so its tool schemas and prompt
//! cache stay intact (`/delegation ... --now` rebuilds on purpose).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[delegation]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent switches of one section
pub struct DelegationConfig {
    /// Children running at once across the profile, synchronous and
    /// background alike; further spawns queue.
    #[schemars(range(min = 1, max = 64))]
    pub max_concurrent_children: u32,
    /// Spawns waiting for a free slot before new ones are refused with
    /// `delegation.at_capacity`.
    #[schemars(range(min = 0, max = 1024))]
    pub max_queued: u32,
    /// Tasks one `delegate_task` call may carry.
    #[schemars(range(min = 1, max = 64))]
    pub max_tasks_per_call: u32,
    /// How deep children may spawn their own children; a child below this
    /// depth keeps the `delegation` toolset (an orchestrator), the others
    /// are leaves.
    #[schemars(range(min = 1, max = 4))]
    pub max_spawn_depth: u32,
    /// Model calls one child may make.
    #[schemars(range(min = 1, max = 1000))]
    pub max_iterations: u32,
    /// Seconds without progress after which a child counts as stalled
    /// (`0` uses the built-in 450 s between iterations and 1200 s inside
    /// one tool call; otherwise at least 30).
    pub inactivity_timeout_secs: u64,
    /// Most characters of one child's summary inserted into the parent's
    /// context (`0` disables the cap); the full text stays readable.
    pub max_summary_chars: u64,
    /// What an approval a child needs does: `deny` it, or `escalate` it to
    /// the root session's surface.
    pub child_approval: ChildApproval,
    /// Seconds an escalated approval waits for an answer before it is
    /// denied.
    #[schemars(range(min = 1, max = 86_400))]
    pub escalation_timeout_secs: u64,
    /// `auto` runs top-level spawns in the background where the session
    /// can receive later completions; `never` always waits.
    pub background: BackgroundMode,
    /// Children a finite one-shot run (`chat -z`, cron, eval) may spawn
    /// (`0` is unlimited); interactive and gateway sessions are never
    /// charged.
    pub oneshot_max_children: u32,
    /// Publish `subagent.*` progress events on the root session's stream.
    pub progress_events: bool,
    /// The delegation router: `default`, or a router a plugin provides.
    /// An unavailable router refuses every spawn.
    pub router: String,
    /// The model children run on unless the task or its agent names one
    /// (`provider/model`); unset: the parent's current route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_concurrent_children: 8,
            max_queued: 32,
            max_tasks_per_call: 8,
            max_spawn_depth: 1,
            max_iterations: 150,
            inactivity_timeout_secs: 0,
            max_summary_chars: 24_000,
            child_approval: ChildApproval::Deny,
            escalation_timeout_secs: 300,
            background: BackgroundMode::Auto,
            oneshot_max_children: 2,
            progress_events: true,
            router: "default".to_owned(),
            model: None,
        }
    }
}

/// How an approval needed inside a child is resolved; no value approves
/// dangerous actions on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChildApproval {
    /// Deny and return the denial to the child.
    #[default]
    Deny,
    /// Ask the root session's surface, tagged with the child's id; denied
    /// when nobody answers in time.
    Escalate,
}

/// When top-level spawns run in the background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundMode {
    /// Where the session can receive later completions.
    #[default]
    Auto,
    /// Never: every call waits for its children.
    Never,
}

/// The `[code_execution]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct CodeExecutionConfig {
    /// Tools a script may call through its stubs; a session sees the
    /// intersection with its own granted tools.
    pub allowed_tools: Vec<String>,
    /// Stub calls one execution may make.
    #[schemars(range(min = 1, max = 10_000))]
    pub max_tool_calls: u32,
    /// Seconds one execution may run.
    #[schemars(range(min = 1, max = 86_400))]
    pub timeout_secs: u64,
}

/// The tools `execute_code` stubs offer by default.
pub const CODE_EXECUTION_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "search_files",
    "patch",
    "terminal",
    "web_search",
    "web_extract",
];

impl Default for CodeExecutionConfig {
    fn default() -> Self {
        Self {
            allowed_tools: CODE_EXECUTION_TOOLS.iter().map(|&t| t.to_owned()).collect(),
            max_tool_calls: 50,
            timeout_secs: 300,
        }
    }
}
