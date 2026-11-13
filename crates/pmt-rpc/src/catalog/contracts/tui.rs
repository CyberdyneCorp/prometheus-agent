//! The contracts the `tui` capability owns: attachment roles and replay,
//! and the discovery and presentation methods every interactive client
//! uses (spec: tui, Session access requires an attachment; Event
//! sequencing and reconnect replay; Discovery RPCs preserve the prompt
//! cache; Composer and path completion; Profile scope on every request).
//! None of them changes a live session's system prompt, toolset or memory
//! snapshot.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Method, SessionRef};

/// What an attached connection may do in the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachRole {
    /// Submits, interrupts and answers the session's requests.
    #[default]
    Driver,
    /// Reads only: receives events, never server requests, and may not
    /// submit (`-32002 session.observer`).
    Observer,
}

/// `session.attach` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachParams {
    /// `ses_<ULID>`.
    pub session_id: String,
    /// Driver (default) or observer.
    #[serde(default)]
    pub role: AttachRole,
    /// The newest sequence number the client applied: the kept events
    /// after it are replayed before the answer.
    #[serde(default)]
    pub last_seen_seq: Option<u64>,
    /// The daemon epoch `last_seen_seq` belongs to; another epoch than the
    /// daemon's answers `truncated: true` and replays nothing.
    #[serde(default)]
    pub epoch: Option<String>,
}

impl AttachParams {
    /// A first attachment as driver.
    #[must_use]
    pub fn driver(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            role: AttachRole::Driver,
            last_seen_seq: None,
            epoch: None,
        }
    }
}

/// Where a command runs from this surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// It runs (`command.dispatch`).
    Exec,
    /// Listed, but refused here with `reason`.
    Unavailable,
    /// Not offered on this surface.
    Hidden,
}

/// Where a command comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    /// A built-in command of the daemon's registry.
    Builtin,
    /// A `[quick_commands]` entry.
    Quick,
    /// An installed skill.
    Skill,
    /// Handled by the client itself (never dispatched).
    Client,
}

/// One `commands.catalog` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandEntry {
    /// The name without `/`.
    pub name: String,
    /// Other names that resolve to it.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// One line.
    pub description: String,
    /// The argument synopsis (`<name> [--now]`), when it takes any.
    #[serde(default)]
    pub args: Option<String>,
    /// Built-in, quick command, skill or client-handled.
    pub source: CommandSource,
    /// Whether it runs from the asking client's surface.
    pub disposition: Disposition,
    /// Why it does not run there.
    #[serde(default)]
    pub reason: Option<String>,
    /// Subcommands with their own disposition on this surface (a palette
    /// offers a command when it or one of them runs here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<SubcommandEntry>,
}

/// One subcommand of a `commands.catalog` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubcommandEntry {
    /// The first word after the command.
    pub name: String,
    /// Whether it runs from the asking client's surface.
    pub disposition: Disposition,
    /// Why it does not run there.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `commands.catalog` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandCatalog {
    /// Every command of the session, by name.
    pub commands: Vec<CommandEntry>,
}

/// `complete.slash` / `complete.path` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteParams {
    /// The session.
    pub session_id: String,
    /// What is typed so far (`/mod` for a command, `./src/ma` for a path,
    /// without the `@` of a mention).
    pub prefix: String,
}

/// One completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    /// The completed text that replaces the prefix.
    pub text: String,
    /// What it is (a command's description).
    #[serde(default)]
    pub description: Option<String>,
    /// A directory (path completion continues inside it).
    #[serde(default)]
    pub dir: bool,
}

/// `complete.slash` / `complete.path` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Completions {
    /// The candidates, best first.
    pub items: Vec<Completion>,
}

/// One `model.options` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelOption {
    /// `<provider>/<model>`.
    pub id: String,
    /// The provider.
    pub provider: String,
    /// The model's context window, when known.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The most reasoning the model takes (`none` when it does not
    /// reason); a higher requested level is clamped to it.
    #[serde(default)]
    pub max_reasoning: Option<String>,
    /// Why switching the session to it is risky (the conversation does
    /// not fit its window, it cannot call tools...); clients confirm.
    #[serde(default)]
    pub risky: Option<String>,
}

/// `model.options` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelOptions {
    /// The session's model (`<provider>/<model>`).
    #[serde(default)]
    pub current: Option<String>,
    /// The models a `/model` switch may pick, current first.
    pub models: Vec<ModelOption>,
}

/// One delegated child of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentSummary {
    /// The child's id.
    pub subagent_id: String,
    /// Its child session.
    pub session_id: String,
    /// `running`, `completed`, `failed` or `stopped`.
    pub status: String,
    /// Its task's title.
    #[serde(default)]
    pub title: Option<String>,
}

/// `subagent.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentList {
    /// The session's children, oldest first.
    pub subagents: Vec<SubagentSummary>,
}

/// `config.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {
    /// The profile; required when the connection is granted several.
    #[serde(default)]
    pub profile: Option<String>,
    /// A dotted key (`model.default`).
    pub key: String,
}

/// `config.get` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigValue {
    /// The key.
    pub key: String,
    /// Its effective value (secrets are never configuration values).
    pub value: Value,
    /// The layer it comes from (`default`, `file`, `set`, `managed`).
    pub source: String,
}

/// The system prompt a session froze on its first turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrozenPrompt {
    /// `sha256:<hex>` of the prompt text.
    pub sha256: String,
    /// The prompt text, its tiers joined in request order.
    pub text: String,
}

/// One MCP server of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerState {
    /// Its configured name.
    pub name: String,
    /// `ready`, `starting`, `failed` or `disabled`.
    pub state: String,
    /// The tools it contributes.
    pub tools: u32,
}

/// `session.banner` result: what the TUI's startup banner lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BannerInfo {
    /// The session's tool names, in request order.
    pub tools: Vec<String>,
    /// The installed skills the session can load.
    pub skills: Vec<String>,
    /// The frozen system prompt; none before the first turn.
    #[serde(default)]
    pub system_prompt: Option<FrozenPrompt>,
    /// The session's MCP servers.
    pub mcp_servers: Vec<McpServerState>,
    /// The degraded marker while the session runs below full hardening,
    /// for example `sandbox: partial:landlock_net` (spec:
    /// security-boundaries, Hardening downgrades are never silent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

method!(/// `commands.catalog`: the session's slash commands with their
    /// disposition on the asking client's surface.
    CommandsCatalog = "commands.catalog" (SessionRef) -> CommandCatalog);
method!(/// `complete.slash`: command names for a typed `/` prefix.
    CompleteSlash = "complete.slash" (CompleteParams) -> Completions);
method!(/// `complete.path`: entries for a typed path, listed through the
    /// session's own terminal backend (empty when it is unreachable).
    CompletePath = "complete.path" (CompleteParams) -> Completions);
method!(/// `model.options`: the models the session may switch to.
    ModelOptionsList = "model.options" (super::ModelOptionsParams) -> ModelOptions);
method!(/// `subagent.list`: the session's delegated children.
    SubagentListing = "subagent.list" (SessionRef) -> SubagentList);
method!(/// `config.get`: one effective configuration value of a profile.
    ConfigGet = "config.get" (ConfigGetParams) -> ConfigValue);
method!(/// `session.banner`: the session's tools, skills, frozen system
    /// prompt and MCP servers (read only; the prompt is never rebuilt).
    SessionBanner = "session.banner" (SessionRef) -> BannerInfo);
