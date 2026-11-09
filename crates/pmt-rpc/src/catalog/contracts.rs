//! Typed contracts: the params and result of each served method, the params
//! and result of each server-to-client request and the payload of each
//! event (spec: workspace-architecture, Contracts generated from types and
//! checked in). `schema/rpc.json` and the TypeScript bindings are generated
//! from these types; a handler can only be registered for a [`Method`], so
//! a method without a contract cannot be served.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A client-to-daemon method.
pub trait Method {
    /// The catalog name.
    const NAME: &'static str;
    /// What the client sends.
    type Params: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    /// What the daemon answers.
    type Result: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

/// A daemon-to-client request.
pub trait ServerRequest {
    /// The catalog name.
    const NAME: &'static str;
    /// What the daemon sends.
    type Params: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    /// What the client answers.
    type Result: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

/// A daemon-to-client event.
pub trait Event {
    /// The catalog name.
    const NAME: &'static str;
    /// The `data` of its envelope.
    type Payload: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

macro_rules! method {
    ($(#[$doc:meta])* $ty:ident = $name:literal ($params:ty) -> $result:ty) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;
        impl Method for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

macro_rules! server_request {
    ($(#[$doc:meta])* $ty:ident = $name:literal ($params:ty) -> $result:ty) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;
        impl ServerRequest for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

macro_rules! event {
    ($(#[$doc:meta])* $ty:ident = $name:literal ($payload:ty)) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;
        impl Event for $ty {
            const NAME: &'static str = $name;
            type Payload = $payload;
        }
    };
}

mod automation;
mod cron;
mod files;
mod journey;
mod local;
mod lsp;
mod messaging;
mod moa;
mod security;
mod tui;
mod turn;
mod upkeep;
mod worktree;
pub use automation::*;
pub use cron::*;
pub use files::*;
pub use journey::*;
pub use local::*;
pub use lsp::*;
pub use messaging::*;
pub use moa::*;
pub use security::*;
pub use tui::*;
pub use turn::*;
pub use upkeep::*;
pub use worktree::*;

/// A `<major>.<minor>` protocol version on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Version {
    /// Incompatible revisions.
    pub major: u16,
    /// Backward-compatible additions.
    pub minor: u16,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl From<pmt_core::protocol::ProtocolVersion> for Version {
    fn from(v: pmt_core::protocol::ProtocolVersion) -> Self {
        Self {
            major: v.major,
            minor: v.minor,
        }
    }
}

/// Which client is attaching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// `prometheus chat` attached to the daemon.
    Cli,
    /// `prometheus tui`.
    Tui,
    /// The desktop app.
    Desktop,
    /// The web dashboard.
    Dashboard,
    /// The ACP adapter.
    Acp,
    /// `prometheus eval`.
    Eval,
}

impl ClientKind {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Desktop => "desktop",
            Self::Dashboard => "dashboard",
            Self::Acp => "acp",
            Self::Eval => "eval",
        }
    }
}

/// What the client can handle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientCapabilities {
    /// The client answers server-to-client requests (approvals, clarify,
    /// secrets, sudo). Without it they resolve unanswered at once.
    #[serde(default)]
    pub server_requests: bool,
    /// Events introduced after the client's minor version that it
    /// understands anyway.
    #[serde(default)]
    pub events: Vec<String>,
}

/// `rpc.hello` params: the first request on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// The attaching client.
    pub client: ClientKind,
    /// Its version.
    pub client_version: String,
    /// The protocol it speaks.
    pub protocol: Version,
    /// What it can handle.
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

/// What the daemon serves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerCapabilities {
    /// The methods with a handler (the rest of the catalog answers
    /// `rpc.not_served`).
    pub methods: Vec<String>,
    /// The server-to-client requests it may send.
    pub server_requests: Vec<String>,
    /// The events it may send this client.
    pub events: Vec<String>,
}

/// The daemon's build target and its support tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlatformSupport {
    /// The Rust target triple.
    pub target: String,
    /// `tier1`, `tier2` or `unsupported`.
    pub tier: String,
}

/// `rpc.hello` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloResult {
    /// The daemon's semantic version.
    pub server_version: String,
    /// The daemon's build id; a client with another one shows the
    /// stale-build notice.
    pub build_id: String,
    /// Random per daemon start; tickets are bound to it.
    pub instance_id: String,
    /// Random per daemon start; event sequence numbers restart with it.
    pub epoch: String,
    /// The protocol the daemon speaks.
    pub protocol: Version,
    /// The profiles the connection's ticket grants.
    pub profiles: Vec<String>,
    /// Hash of the daemon's `schema/rpc.json`.
    pub methods_digest: String,
    /// What the daemon serves.
    pub capabilities: ServerCapabilities,
    /// The daemon's target and tier.
    pub platform_support: PlatformSupport,
}

/// No params.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// `{"ok": true}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    /// Always true.
    pub ok: bool,
}

impl Ack {
    /// `{"ok": true}`.
    pub const YES: Self = Self { ok: true };
}

/// A session named by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    /// `ses_<ULID>`.
    pub session_id: String,
}

/// `session.create` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    /// The profile; required only when the ticket grants several.
    #[serde(default)]
    pub profile: Option<String>,
    /// The working directory of the session.
    #[serde(default)]
    pub cwd: Option<String>,
    /// A source tag recorded on the session.
    #[serde(default)]
    pub source_tag: Option<String>,
    /// The client creating it; must be the connection's own `client`
    /// kind (`-32002 rpc.source_mismatch` otherwise). Absent: the
    /// connection's kind.
    #[serde(default)]
    pub source: Option<ClientKind>,
    /// Retries with the same key within 300 s return the same session
    /// instead of creating another one.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Make this connection the session's interactive owner: another
    /// connection resuming it exclusively is refused while it is attached.
    #[serde(default)]
    pub exclusive: bool,
    /// How the session runs (a chat's flags).
    #[serde(default)]
    pub setup: SessionSetup,
}

/// `session.resume` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeParams {
    /// `ses_<ULID>`.
    pub session_id: String,
    /// Become the session's interactive owner; fails with `-32004
    /// session.in_use` naming the owner's pid while another connection is.
    #[serde(default)]
    pub exclusive: bool,
    /// How the session runs from now on; a loaded, idle session is
    /// reloaded with it. Absent: as it is (or the profile's defaults).
    #[serde(default)]
    pub setup: Option<SessionSetup>,
}

/// How a daemon session runs: the session-shaping flags of `prometheus
/// chat`, applied when the session loads (spec: cli, Daemon attachment
/// with in-process fallback).
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent chat switches
pub struct SessionSetup {
    /// The model (`[<provider>/]<model>` or an alias) instead of the
    /// profile's (`-m`).
    #[serde(default)]
    pub model: Option<String>,
    /// The provider instead of the profile's (`--provider`).
    #[serde(default)]
    pub provider: Option<String>,
    /// `key=value` configuration overrides for this session only (`--set`,
    /// `--approvals`, `--max-iterations`, `--toolsets`).
    #[serde(default)]
    pub set: Vec<String>,
    /// Skills preloaded into the session (`--skills`).
    #[serde(default)]
    pub skills: Vec<String>,
    /// Approval-gated calls run without asking (`--yolo`).
    #[serde(default)]
    pub yolo: bool,
    /// The system prompt names the session id (`--pass-session-id`).
    #[serde(default)]
    pub pass_session_id: bool,
    /// No `AGENTS.md`-style context files (`--no-context-files`).
    #[serde(default)]
    pub no_context_files: bool,
    /// Schema defaults instead of the profile's `config.toml`
    /// (`--ignore-user-config`).
    #[serde(default)]
    pub ignore_user_config: bool,
    /// No rule files, identity file, memory snapshot or preloaded skills
    /// (`--ignore-rules`).
    #[serde(default)]
    pub ignore_rules: bool,
    /// A mock provider script on the daemon's machine (`--mock-script`).
    #[serde(default)]
    pub mock_script: Option<String>,
    /// An endpoint for this session only (`--base-url`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// That endpoint's API key (`--api-key-stdin`), held in memory only.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for SessionSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSetup")
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("set", &self.set)
            .field("skills", &self.skills)
            .field("yolo", &self.yolo)
            .field("pass_session_id", &self.pass_session_id)
            .field("no_context_files", &self.no_context_files)
            .field("ignore_user_config", &self.ignore_user_config)
            .field("ignore_rules", &self.ignore_rules)
            .field("mock_script", &self.mock_script)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// What `session.resolve` looks up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolveKind {
    /// `-r <ref>`: an id, an id prefix, a title or `latest`.
    Resume,
    /// `-c [<name>]`: the newest CLI session of the directory (empty
    /// reference) or the newest session of the lineage titled `name`.
    Continue,
}

/// `session.resolve` params: a chat's `-r`/`-c` reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    /// The profile; required only when the ticket grants several.
    #[serde(default)]
    pub profile: Option<String>,
    /// Resume or continue.
    pub kind: ResolveKind,
    /// The reference (empty for a bare `-c`).
    pub reference: String,
    /// The client's working directory (scopes `-c` and `latest`).
    #[serde(default)]
    pub cwd: Option<String>,
    /// The client terminal's identity (prefers the session last used from
    /// it for a bare `-c`).
    #[serde(default)]
    pub terminal: Option<String>,
}

/// `session.fork` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkParams {
    /// The session to fork.
    pub session_id: String,
    /// The last message the fork keeps (`msg_<ULID>`); default the
    /// session's last active message.
    #[serde(default)]
    pub at_message_id: Option<String>,
    /// The fork's title; default `<parent title> #<n>`.
    #[serde(default)]
    pub title: Option<String>,
}

/// One related session in `session.tree` / `session.siblings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LineageNode {
    /// The session.
    pub session_id: String,
    /// Its parent.
    pub parent_session_id: String,
    /// How it relates to the parent: `fork`, `compression`, `delegate`,
    /// `reset`, `import` or `background`.
    pub kind: String,
    /// Distance from the asked session (its children are 1).
    pub depth: u32,
    /// Its title.
    #[serde(default)]
    pub title: Option<String>,
}

/// `session.tree` / `session.siblings` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
    /// The related sessions, by depth then age.
    pub nodes: Vec<LineageNode>,
}

/// `session.attach` / `session.resume` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attached {
    /// The session.
    pub session_id: String,
    /// The newest event sequence number kept for it.
    pub latest_seq: u64,
    /// The daemon epoch the sequence numbers belong to.
    pub epoch: String,
    /// Events after the client's `last_seen_seq` were evicted, or the
    /// client's epoch is not this daemon's: reload the transcript
    /// (`session.transcript`) instead of applying the replayed events.
    #[serde(default)]
    pub truncated: bool,
    /// Requests of the session still waiting for an answer (re-sent to
    /// this connection when it is a capable driver).
    #[serde(default)]
    pub open_requests: Vec<OpenRequest>,
}

/// `session.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    /// The profile; required only when the ticket grants several.
    #[serde(default)]
    pub profile: Option<String>,
    /// At most this many, newest activity first (default 50).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One listed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    /// `ses_<ULID>`.
    pub session_id: String,
    /// Its title, when it has one.
    pub title: Option<String>,
    /// When it started (RFC 3339).
    pub started_at: String,
    /// Whether a turn is running in the daemon.
    pub live: bool,
}

/// `session.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionList {
    /// The sessions.
    pub sessions: Vec<SessionSummary>,
}

/// `session.events.since` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsSinceParams {
    /// The session.
    pub session_id: String,
    /// The last sequence number the client saw (0 for none).
    pub last_seen: u64,
}

/// One event frame: the notification params and the replay item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// The event name.
    pub event: String,
    /// The session it belongs to; `None` for daemon-wide events.
    pub session_id: Option<String>,
    /// Per-session monotonic sequence number (per daemon for daemon-wide
    /// events).
    pub seq: u64,
    /// The daemon epoch.
    pub epoch: String,
    /// The payload, as the event's contract defines it.
    pub data: Value,
}

/// A server-to-client request still waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenRequest {
    /// `srq-<12 hex>`.
    pub id: String,
    /// The request name (`approval.request`, ...).
    pub method: String,
    /// Its params.
    pub params: Value,
}

/// `session.events.since` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsSince {
    /// The kept events after `last_seen`, oldest first.
    pub events: Vec<Envelope>,
    /// The newest sequence number of the session.
    pub latest_seq: u64,
    /// Events after `last_seen` were evicted.
    pub truncated: bool,
    /// The daemon epoch; a different one than the client's means the
    /// daemon restarted and the client resets its watermark.
    pub epoch: String,
    /// Requests of the session still waiting for an answer.
    pub open_requests: Vec<OpenRequest>,
}

/// One message of a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptMessage {
    /// `msg_<ULID>`.
    pub id: String,
    /// `user`, `assistant`, `tool` or `system`.
    pub role: String,
    /// Its text, when it has any.
    pub text: Option<String>,
}

/// `session.transcript` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    /// The active messages, oldest first.
    pub messages: Vec<TranscriptMessage>,
}

/// `session.compact` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactParams {
    /// The session.
    pub session_id: String,
    /// What the summary should keep in focus.
    #[serde(default)]
    pub focus: Option<String>,
}

/// `session.compact` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Compacted {
    /// How the compaction ended.
    pub status: String,
    /// Context tokens before.
    pub tokens_before: u64,
    /// Context tokens after.
    pub tokens_after: u64,
}

/// `session.context_usage` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    /// Estimated tokens of the active context.
    pub tokens: u64,
    /// The model's context window, when known.
    pub context_window: Option<u64>,
    /// The session's cost so far in the usage ledger's currency (USD),
    /// priced calls only.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// The current route's `billing_source` (`subscription:<provider>/<label>`,
    /// `api_key:<provider>[/<label>]`, `local`...), when the session is
    /// loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_source: Option<String>,
    /// The subscription the current route bills, when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionStatus>,
}

/// A subscription's status in `session.context_usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionStatus {
    /// Its plan and quota windows.
    pub quota: QuotaStatus,
}

/// A subscription's plan and quota windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotaStatus {
    /// The provider reports quota data.
    pub reported: bool,
    /// The plan label.
    #[serde(default)]
    pub plan: Option<String>,
    /// The quota windows.
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    /// One line for a status display (`quota: not reported by provider`).
    pub text: String,
}

/// One quota window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    /// `5h`, `weekly`...
    pub name: String,
    /// Used share, 0-100.
    pub used_percent: f64,
    /// When it resets (RFC 3339).
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// `prompt.submit` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitParams {
    /// The session.
    pub session_id: String,
    /// The prompt.
    pub text: String,
    /// Images that go with it.
    #[serde(default)]
    pub images: Vec<PromptImage>,
    /// Other files received with it (absolute paths on the daemon's
    /// machine): kept in the session's blob store and, with
    /// `attachments.storage = "workspace"`, copied into the workspace.
    #[serde(default)]
    pub files: Vec<String>,
    /// Wall-clock budget of this turn in seconds (`--run-budget`).
    #[serde(default)]
    pub run_budget_secs: Option<u64>,
    /// The client's id of this input: a resend with the same id returns
    /// the original admission instead of running a second turn.
    #[serde(default)]
    pub input_id: Option<String>,
    /// Never accepted: the gateway stamps every input's author from the
    /// connection, so a client-supplied one is refused with `-32002
    /// rpc.forged_author`.
    #[serde(default)]
    pub author: Option<Value>,
}

/// An image attached to a prompt: a file the daemon reads (checked like
/// `chat --image`), or an image the session already holds (as
/// `command.dispatch` returns them for `/retry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum PromptImage {
    /// An absolute path on the daemon's machine.
    Path(String),
    /// A blob reference (`blob:sha256:<hex>`) of one of the session's
    /// earlier user messages.
    Blob(String),
}

/// The `session.event` type of a notice a command posts after it answered
/// (`/refine`'s summary, a skill's load notice): `payload.text` is the
/// text to show. Every other `session.event` is a trajectory event.
pub const COMMAND_NOTICE: &str = "command.notice";

/// The `session.event` type of a background process notice of the session
/// (an exit, a watched line, a heartbeat; spec: execution-environments,
/// Background notifications are throttled, Quiet-wait auto-notify):
/// `payload.kind` is `result` (exit 0), `error` or `progress`, and
/// `payload.text` is the text to show as its own message.
pub const PROCESS_NOTICE: &str = "process.notice";

/// `command.dispatch` params: a slash command the client does not handle
/// itself (spec: tui, Slash command flow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchParams {
    /// The session.
    pub session_id: String,
    /// The command name without `/` (an alias or unique prefix resolves).
    pub name: String,
    /// The rest of the line, case kept.
    #[serde(default)]
    pub args: String,
}

/// `command.dispatch` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Dispatched {
    /// The command ran; `notice` is its output to show.
    Done {
        /// What to show (may be empty).
        notice: String,
    },
    /// Submit `message` as a prompt (with `images`).
    Send {
        /// The prompt.
        message: String,
        /// Images that go with it.
        #[serde(default)]
        images: Vec<PromptImage>,
    },
    /// The command did not run.
    Refused {
        /// Why.
        reason: String,
    },
}

/// `prompt.submit` result: the input was admitted; its turn's output
/// arrives as events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Submitted {
    /// The admission id: the turn that runs the input (for a steered
    /// input, the running turn it joined).
    pub turn_id: String,
    /// How the input was admitted.
    #[serde(default)]
    pub admission: Admission,
    /// What a `handled` input did, to show in place of a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// How `prompt.submit` admitted an input (spec: tui, Prompt submission
/// while busy).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// Its turn started.
    #[default]
    Started,
    /// A turn was running: it runs after the queued inputs before it
    /// (`busy.input_mode = "queue"`, or `interrupt`, which stops the
    /// running turn first).
    Queued,
    /// Handed to the running turn after its current tool step
    /// (`busy.input_mode = "steer"`).
    Steered,
    /// Answered without a turn: the `yes` that saves the job a `/cron`
    /// echo just showed (spec: scheduled-jobs, /cron slash command); the
    /// result's `notice` says what happened.
    Handled,
}

/// An approval answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow this call.
    Once,
    /// Allow matching calls for the rest of the session.
    Session,
    /// Allow matching calls from now on (recorded in `grants.toml`).
    Always,
    /// Refuse.
    Deny,
    /// Show the parsed command, the rule, the paths it touches and a
    /// dry-run preview, then ask again; never an approval.
    Explain,
}

/// The answer to `approval.request` (also `approval.respond` params,
/// with its `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAnswer {
    /// The decision.
    pub decision: Decision,
    /// For a diff approval: the hunks accepted.
    #[serde(default)]
    pub accepted_hunks: Option<Vec<u32>>,
    /// An edited command to run instead.
    #[serde(default)]
    pub replacement: Option<String>,
}

/// `approval.respond` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondParams {
    /// The `approval.request` id (`srq-<12 hex>`).
    pub id: String,
    /// The decision.
    pub decision: Decision,
    /// For a diff approval: the hunks accepted.
    #[serde(default)]
    pub accepted_hunks: Option<Vec<u32>>,
    /// An edited command to run instead.
    #[serde(default)]
    pub replacement: Option<String>,
}

/// Why a call needs approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalFinding {
    /// The detector (`dangerous-pattern`, `protected-file`, ...).
    pub detector: String,
    /// What it found.
    pub reason: String,
    /// The offending part, invisible characters escaped.
    pub fragment: String,
}

/// `approval.request` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestParams {
    /// The session.
    pub session_id: String,
    /// The tool called.
    pub tool: String,
    /// The model's call id.
    pub call_id: String,
    /// The command line or written path.
    pub subject: Option<String>,
    /// Why approval is needed.
    pub findings: Vec<ApprovalFinding>,
    /// What `session` and `always` record.
    pub remember: Option<String>,
    /// Milliseconds until the request settles unanswered (set by the
    /// daemon when it sends the request).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// What an `explain` answer produced: the parsed command, the rule,
    /// the paths it touches and a dry-run preview (the request is asked
    /// again with it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// `clarify.request` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClarifyRequestParams {
    /// The session.
    pub session_id: String,
    /// The questions, as the `clarify` tool defines them.
    pub questions: Vec<Value>,
    /// Surfaces render a free-text `Other` option.
    pub allow_other: bool,
    /// Milliseconds until the request settles unanswered (set by the
    /// daemon when it sends the request).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// `clarify.request` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClarifyAnswers {
    /// One answer per question, as the `clarify` tool defines them.
    pub answers: Vec<Value>,
}

/// `secret.request` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretRequestParams {
    /// The session.
    pub session_id: String,
    /// The secret's name.
    pub name: String,
    /// Why it is needed.
    pub reason: Option<String>,
    /// Milliseconds until the request settles unanswered (set by the
    /// daemon when it sends the request).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// `secret.request` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretAnswer {
    /// The value; `None` declines.
    pub value: Option<String>,
}

/// `sudo.request` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SudoRequestParams {
    /// The session.
    pub session_id: String,
    /// The command that needs the password.
    pub command: String,
    /// Milliseconds until the request settles unanswered (set by the
    /// daemon when it sends the request).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// `sudo.request` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SudoAnswer {
    /// The password; `None` declines.
    pub password: Option<String>,
}

/// `request.cancel` payload: a server request timed out or was cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestCancel {
    /// `srq-<12 hex>`.
    pub id: String,
    /// `timeout`, `cancelled`, or `no_responder` (no attached driver can
    /// answer: none was attached when it was asked, or the last one left).
    pub reason: String,
}

/// `request.resolved` payload: a client answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestResolved {
    /// `srq-<12 hex>`.
    pub id: String,
    /// The client that answered (`<client kind>-<connection number>`).
    pub by_client: String,
}

/// Why a turn ended in error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnError {
    /// The error taxonomy kind (it fixes a CLI's exit status).
    pub kind: String,
    /// The stable dotted error code.
    pub code: String,
    /// One line.
    pub message: String,
    /// The command that fixes it.
    pub hint: Option<String>,
}

impl TurnError {
    /// The wire form of `err`.
    #[must_use]
    pub fn of(err: &pmt_core::Error) -> Self {
        Self {
            kind: err.kind().as_str().to_owned(),
            code: err.code().code.to_owned(),
            message: err.message().to_owned(),
            hint: err.hint().map(str::to_owned),
        }
    }

    /// Back to an error of its catalog code (`rpc.connection` for a code
    /// this build does not know).
    #[must_use]
    pub fn to_error(&self) -> pmt_core::Error {
        let catalog = pmt_core::error::ERROR_CODES
            .iter()
            .find(|c| c.code == self.code)
            .copied()
            .unwrap_or(&pmt_core::error::codes::RPC_CONNECTION);
        let err = pmt_core::Error::new(catalog, self.message.clone());
        match &self.hint {
            Some(hint) => err.with_hint(hint.clone()),
            None => err,
        }
    }
}

/// `turn.finished` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnFinished {
    /// The turn.
    pub turn_id: String,
    /// The turn's exit reason (`completed`, `interrupted_by_user`, ...).
    pub exit_reason: String,
    /// The final answer, when there is one.
    pub final_response: Option<String>,
    /// The error that ended the turn.
    pub error: Option<TurnError>,
    /// The turn's token and cost totals, as the turn loop reports them.
    #[serde(default)]
    pub usage: Option<Value>,
    /// The turn's `--usage-file` report (session, lineage root, route,
    /// billing source, tokens, cost and cost status of its model calls).
    #[serde(default)]
    pub report: Option<Value>,
}

impl JsonSchema for crate::frame::ErrorData {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ErrorData".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        #[derive(JsonSchema)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)] // only its schema is used
        struct ErrorData {
            kind: String,
            code: String,
            hint: Option<String>,
            path: Option<String>,
        }
        ErrorData::json_schema(generator)
    }
}

method!(/// `rpc.hello`.
    Hello = "rpc.hello" (HelloParams) -> HelloResult);
method!(/// `rpc.ping`, answered from the read loop.
    Ping = "rpc.ping" (Empty) -> Ack);
method!(/// `session.create`.
    SessionCreate = "session.create" (SessionCreateParams) -> SessionRef);
method!(/// `session.attach`: attaches the connection (replaying the kept
    /// events after `last_seen_seq`).
    SessionAttach = "session.attach" (AttachParams) -> Attached);
method!(/// `session.detach`.
    SessionDetach = "session.detach" (SessionRef) -> Ack);
method!(/// `session.list`.
    SessionListing = "session.list" (SessionListParams) -> SessionList);
method!(/// `session.resume`: loads the session into the daemon and attaches.
    SessionResume = "session.resume" (ResumeParams) -> Attached);
method!(/// `session.interrupt`: stops the session's running turn at its next
    /// safe point (the partial turn is kept, marked interrupted).
    SessionInterrupt = "session.interrupt" (SessionRef) -> Ack);
method!(/// `session.resolve`: the session a `-r`/`-c` reference names.
    SessionResolve = "session.resolve" (ResolveParams) -> SessionRef);
method!(/// `session.fork`: a new session with the parent's active messages up
    /// to `at_message_id`, the parent unchanged.
    SessionFork = "session.fork" (ForkParams) -> SessionRef);
method!(/// `session.tree`: every descendant of a session (forks, compression
    /// continuations, delegates) with its relation.
    SessionTree = "session.tree" (SessionRef) -> Lineage);
method!(/// `session.siblings`: the other children of a session's parent.
    SessionSiblings = "session.siblings" (SessionRef) -> Lineage);
method!(/// `session.compact`.
    SessionCompact = "session.compact" (CompactParams) -> Compacted);
method!(/// `session.transcript`.
    SessionTranscript = "session.transcript" (SessionRef) -> Transcript);
method!(/// `session.events.since`.
    SessionEventsSince = "session.events.since" (EventsSinceParams) -> EventsSince);
method!(/// `session.context_usage`.
    SessionContextUsage = "session.context_usage" (SessionRef) -> ContextUsage);
method!(/// `prompt.submit`.
    PromptSubmit = "prompt.submit" (SubmitParams) -> Submitted);
method!(/// `command.dispatch`: runs a slash command in the session.
    CommandDispatch = "command.dispatch" (DispatchParams) -> Dispatched);
method!(/// `approval.respond`.
    ApprovalRespond = "approval.respond" (ApprovalRespondParams) -> Ack);

server_request!(/// `approval.request`.
    ApprovalAsk = "approval.request" (ApprovalRequestParams) -> ApprovalAnswer);
server_request!(/// `clarify.request`.
    ClarifyAsk = "clarify.request" (ClarifyRequestParams) -> ClarifyAnswers);
server_request!(/// `secret.request`.
    SecretAsk = "secret.request" (SecretRequestParams) -> SecretAnswer);
server_request!(/// `sudo.request`.
    SudoAsk = "sudo.request" (SudoRequestParams) -> SudoAnswer);

event!(/// `request.cancel`.
    RequestCancelled = "request.cancel" (RequestCancel));
event!(/// `request.resolved`.
    RequestDone = "request.resolved" (RequestResolved));
event!(/// `session.event`: one trajectory event of the session (the shared
    /// trajectory grammar, `schema/trajectory/v1.json`).
    SessionEvent = "session.event" (Value));
event!(/// `turn.finished`.
    TurnEnded = "turn.finished" (TurnFinished));
