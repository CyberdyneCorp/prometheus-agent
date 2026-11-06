//! The `[delegation]` and `[code_execution]` sections (spec:
//! subagent-delegation, Delegation configuration defaults; Programmatic
//! tool calling under the caller's grant; `execute_code` RPC channel and
//! limits). Both apply from the next session: a running session keeps the
//! delegation settings it started with, so its tool schemas and prompt
//! cache stay intact (`/delegation ... --now` rebuilds on purpose).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{SecretName, WireFormat};

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
    /// Run each child in its own git worktree and branch when the parent
    /// uses the local terminal backend inside a git repository.
    pub worktree_isolation: bool,
    /// Prefix of a child's worktree branch (`<prefix>/subagent-<id>`).
    pub worktree_branch_prefix: String,
    /// A direct endpoint children call instead of a provider route
    /// (`model`, or the parent's model id, names the model there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Name of the secret holding the direct endpoint's key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret: Option<SecretName>,
    /// Wire format of the direct endpoint (default: inferred from its
    /// URL, as for a custom provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_format: Option<WireFormat>,
    /// Request parameters set on every child model call, winning over the
    /// route's own; the `extra_body` table is merged one level deep into
    /// the request body.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_overrides: BTreeMap<String, serde_json::Value>,
    /// How messaging sessions run their children: `auto` waits for them
    /// inside the call when the platform's interim assistant messages are
    /// off, `inline` always waits, `background` never does.
    pub gateway_mode: GatewayDelegationMode,
    /// Deliver each ungrouped task's result (and each `group`'s results)
    /// as its own completion as soon as it is ready.
    pub independent_completions: bool,
    /// The model `/review` runs on (unset: the session's route).
    pub review: ReviewConfig,
}

/// `[delegation.review]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    /// Provider of the reviewer's model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The reviewer's model (with `provider`, a model id there; alone,
    /// `provider/model`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// How a messaging session runs its children.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDelegationMode {
    /// Wait inside the call where interim assistant messages are off.
    #[default]
    Auto,
    /// Always wait inside the call.
    Inline,
    /// Never wait: children report as later completions.
    Background,
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
            worktree_isolation: false,
            worktree_branch_prefix: "prometheus".to_owned(),
            base_url: None,
            api_key_secret: None,
            wire_format: None,
            request_overrides: BTreeMap::new(),
            gateway_mode: GatewayDelegationMode::Auto,
            independent_completions: false,
            review: ReviewConfig::default(),
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
    /// `project` runs scripts in the session's working directory on the
    /// project's virtual environment (3.8 or newer) when there is one;
    /// `strict` runs them in a fresh staging directory on the managed
    /// Python runtime.
    pub mode: CodeExecutionMode,
    /// Seconds an unused Python kernel stays alive before it is stopped.
    #[schemars(range(min = 1, max = 604_800))]
    pub kernel_idle_timeout_secs: u64,
    /// Top-level sessions keeping a Python kernel at once; the least
    /// recently used one loses its kernel beyond this (children's kernels
    /// are not counted).
    #[schemars(range(min = 1, max = 64))]
    pub max_session_kernels: u32,
}

/// Where `execute_code` runs a script and on which Python.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeExecutionMode {
    /// The session's working directory and the project's virtual
    /// environment, else the managed runtime.
    #[default]
    Project,
    /// A fresh staging directory and the managed runtime.
    Strict,
}

impl CodeExecutionMode {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Strict => "strict",
        }
    }
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
            mode: CodeExecutionMode::Project,
            kernel_idle_timeout_secs: 1800,
            max_session_kernels: 4,
        }
    }
}
