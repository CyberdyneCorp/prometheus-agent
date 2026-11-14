//! The desktop's management panes (spec: desktop-app, Management panes;
//! Messaging pane shows saved versus running state; Telegram quick setup
//! by QR; Memory graph): the same catalogs and changes the dashboard's
//! pages reach, each naming the profile it targets, the messaging
//! adapters' saved-versus-running state, and the command center.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// `manage.catalog` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManageCatalogParams {
    /// The profile the pane targets.
    pub profile: String,
    /// `skills`, `optional_skills`, `toolsets`, `models`, `plugins`,
    /// `mcp`, `cron`, `cron_runs` (of the job `target` names) or `memory`.
    pub catalog: String,
    /// The job whose runs `cron_runs` lists.
    #[serde(default)]
    pub target: Option<String>,
}

/// `manage.change` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManageChangeParams {
    /// The profile that owns what changes (a job's owning profile in the
    /// all-profiles view).
    pub profile: String,
    /// `skill` (`{"enabled"}`), `skill_archive`, `toolset`, `cron_update`
    /// (`{"enabled"}` pauses or resumes), `cron_delete`, `cron_run` or
    /// `memory` (`{"replace"|"remove": ...}`).
    pub change: String,
    /// The skill, toolset or job.
    #[serde(default)]
    pub target: Option<String>,
    /// The change's fields.
    #[serde(default)]
    pub body: Value,
}

method!(/// `manage.catalog`: one catalog of a profile, as the dashboard
    /// lists it.
    ManageCatalog = "manage.catalog" (ManageCatalogParams) -> Value);
method!(/// `manage.change`: one change of a profile through the code its CLI
    /// command runs; it applies to live sessions from their next session.
    ManageChange = "manage.change" (ManageChangeParams) -> Value);

/// One messaging adapter of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterRow {
    /// The platform.
    pub platform: String,
    /// Its display name.
    pub display: String,
    /// `not_configured`, `saved` (credentials stored, disabled),
    /// `pending_restart` (configuration changed since it started),
    /// `connecting`, `connected`, `error` or `daemon_stopped`.
    pub state: String,
    /// Why, for `error`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Enabled in the configuration.
    pub enabled: bool,
    /// Its secrets and whether each is set (never values).
    pub secrets: BTreeMap<String, bool>,
    /// Its interactive pairing flow (`telegram_qr`, `whatsapp_qr`).
    #[serde(default)]
    pub pairing: Option<String>,
}

/// `adapters.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterList {
    /// Every adapter available to the profile.
    pub adapters: Vec<AdapterRow>,
}

/// `adapters.save` params: credentials and the enable switch, written
/// together; nothing restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterSaveParams {
    /// The profile.
    pub profile: String,
    /// The platform.
    pub platform: String,
    /// The new enable switch.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Secrets to store (write-only).
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// Secrets to delete.
    #[serde(default)]
    pub clear: Vec<String>,
}

/// An adapter of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterRef {
    /// The profile.
    pub profile: String,
    /// The platform.
    pub platform: String,
}

method!(/// `adapters.list`: each adapter's saved-versus-running state.
    AdaptersList = "adapters.list" (super::ProfileParams) -> AdapterList);
method!(/// `adapters.save`: credentials and the enable switch (it restarts
    /// nothing).
    AdaptersSave = "adapters.save" (AdapterSaveParams) -> AdapterRow);
method!(/// `adapters.restart`: restarts that adapter alone from its saved
    /// configuration; its state once settled.
    AdaptersRestart = "adapters.restart" (AdapterRef) -> AdapterRow);
method!(/// `adapters.pairing.start`: starts the adapter's pairing flow
    /// (Telegram Create with QR).
    AdaptersPairingStart = "adapters.pairing.start" (AdapterRef) -> Value);
method!(/// `adapters.pairing.state`: the flow's current step or outcome.
    AdaptersPairingState = "adapters.pairing.state" (AdapterRef) -> Value);
method!(/// `adapters.pairing.cancel`: stops the flow; a late result is
    /// discarded and nothing is saved.
    AdaptersPairingCancel = "adapters.pairing.cancel" (AdapterRef) -> super::Ack);

/// A session with a running turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenterSession {
    /// Its profile.
    pub profile: String,
    /// The session.
    pub session_id: String,
    /// Its title.
    #[serde(default)]
    pub title: Option<String>,
}

/// A pending approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenterApproval {
    /// Its session's profile.
    pub profile: String,
    /// The session.
    pub session_id: String,
    /// The request.
    pub request_id: String,
    /// What is asked (the command or the tool).
    pub summary: String,
}

/// An active subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenterSubagent {
    /// Its parent's profile.
    pub profile: String,
    /// The parent session.
    pub session_id: String,
    /// The child.
    pub subagent_id: String,
    /// Its task.
    pub goal: String,
}

/// `center.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenterList {
    /// Running sessions across the connection's profiles.
    pub sessions: Vec<CenterSession>,
    /// Pending approvals.
    pub approvals: Vec<CenterApproval>,
    /// Active subagents.
    pub subagents: Vec<CenterSubagent>,
}

/// `center.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenterParams {}

method!(/// `center.list`: every running session, pending approval and active
    /// subagent across the connection's profiles.
    CenterListCall = "center.list" (CenterParams) -> CenterList);
