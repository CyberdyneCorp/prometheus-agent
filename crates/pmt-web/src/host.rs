//! What the dashboard needs from the daemon beyond the client protocol: a
//! served profile's configuration and store, its catalogs and redaction.
//! The daemon implements it; profile data is reached only through it, so
//! an unserved or unknown profile is never opened by the dashboard.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_gateway::HostFuture;
use pmt_store::Store;
use serde_json::Value;

/// A catalog of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    /// Its skills with their enabled state.
    Skills,
    /// Its toolsets with their enabled state.
    Toolsets,
    /// The provider/model pairs it is configured with.
    Models,
    /// Its plugins with their load state and problem.
    Plugins,
    /// Its configured MCP servers.
    Mcp,
    /// Its cron jobs with their state and next run.
    Cron,
    /// Its memory entries.
    Memory,
    /// Its webhook subscriptions (never their secrets).
    Webhooks,
}

/// A change of a profile's state the dashboard asks the daemon for; the
/// target (`name`/`id`) and JSON body come with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// `PATCH skills/{name} {"enabled": bool}`.
    Skill,
    /// Archives a skill of the profile (the desktop's memory graph
    /// deletes a skill node this way; `curator archive`).
    SkillArchive,
    /// `PATCH toolsets/{name} {"enabled": bool}`.
    Toolset,
    /// `POST cron` (the `cron create` fields).
    CronCreate,
    /// `PATCH cron/{id}` (the `cron edit` fields, `enabled`).
    CronUpdate,
    /// `DELETE cron/{id}`.
    CronDelete,
    /// `POST cron/{id}/run`.
    CronRun,
    /// `PATCH memory {"add"|"replace"|"remove": ...}`.
    Memory,
    /// `POST webhooks`: the answer carries the HMAC secret, once.
    WebhookCreate,
    /// `PATCH webhooks/{name} {"enabled": bool}`.
    WebhookUpdate,
    /// `DELETE webhooks/{name}`.
    WebhookDelete,
    /// `POST webhooks/{name}/test`.
    WebhookTest,
    /// `POST maintenance/{action}`, confirmed.
    Maintenance,
}

/// A host file of a session, read under its file-access rules.
#[derive(Debug, Clone)]
pub struct SessionFile {
    /// The file name (for `Content-Disposition`).
    pub name: String,
    /// Its bytes.
    pub bytes: Vec<u8>,
}

/// Where an interactive adapter pairing flow reports its steps (each a
/// JSON object with a `state`: `qr` with an `image` data URI, `code`...),
/// and how it learns that the user closed it: a cancelled flow stops and
/// writes nothing.
#[derive(Clone)]
pub struct PairingSink {
    emit: Arc<dyn Fn(Value) + Send + Sync>,
    cancel: pmt_core::cancel::CancelToken,
}

impl std::fmt::Debug for PairingSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingSink").finish_non_exhaustive()
    }
}

impl PairingSink {
    /// A sink calling `emit` for each step, cancelled by `cancel`.
    pub fn new(
        emit: Arc<dyn Fn(Value) + Send + Sync>,
        cancel: pmt_core::cancel::CancelToken,
    ) -> Self {
        Self { emit, cancel }
    }

    /// Reports one step.
    pub fn emit(&self, step: Value) {
        (self.emit)(step);
    }

    /// Cancelled when the user closes the flow.
    #[must_use]
    pub fn cancel(&self) -> &pmt_core::cancel::CancelToken {
        &self.cancel
    }
}

/// The daemon side of the dashboard.
pub trait DashboardHost: Send + Sync {
    /// The served profile's configuration now (`None`: not served).
    fn scope(&self, profile: &str) -> Option<ProfileScope>;

    /// The served profile's store (`None`: not served).
    fn store(&self, profile: &str) -> Option<Arc<Store>>;

    /// The profile the daemon was started with (machine-wide dashboard
    /// credentials live in its secrets store).
    fn launch_profile(&self) -> String;

    /// `text` with the profile's known secrets (every served profile's for
    /// `None`) and credential-shaped tokens redacted.
    fn redact(&self, profile: Option<&str>, text: &str) -> String;

    /// A catalog of the profile.
    fn catalog<'a>(&'a self, profile: &'a str, what: Catalog) -> HostFuture<'a, Value>;

    /// Creates a profile as the profile builder describes it (`name`,
    /// `description`, `clone_from`, `provider`, `model`, `soul`,
    /// `toolsets`, `skills_disabled`, `adapters`); nothing is left behind
    /// when a step fails. The daemon serves it afterwards.
    fn create_profile(&self, body: Value) -> HostFuture<'_, Value>;

    /// Host file `path` of session `session` (relative to its working
    /// directory), read only through the session's file-access rules: the
    /// guards and capability checks its file tools apply.
    fn session_file<'a>(
        &'a self,
        profile: &'a str,
        session: &'a str,
        path: &'a str,
    ) -> HostFuture<'a, SessionFile>;

    /// Runs an adapter's interactive pairing `flow` for the profile
    /// (`WhatsApp` QR linking, Telegram Create with QR), reporting each step
    /// to `sink`; on success it saves what the flow obtained, restarts
    /// that adapter alone and answers the final step.
    fn pair_adapter<'a>(
        &'a self,
        profile: &'a str,
        flow: pmt_config::platforms::PairingFlow,
        sink: PairingSink,
    ) -> HostFuture<'a, Value>;

    /// The dashboard contributions of the profile's plugins as discovery
    /// finds them now: `{"plugins": [...], "ignored": [...]}` in load
    /// order (`pmt_plugins::dashboard::Contributions`).
    fn dashboard_plugins<'a>(&'a self, profile: &'a str) -> HostFuture<'a, Value>;

    /// Reloads the profile's plugin discovery and live plugin set.
    fn rescan_plugins<'a>(&'a self, profile: &'a str) -> HostFuture<'a, ()>;

    /// The HTML document plugin `id`'s frames load.
    fn plugin_frame<'a>(&'a self, profile: &'a str, id: &'a str) -> HostFuture<'a, String>;

    /// Runs backend `request` of plugin `id` in the plugin host (never in
    /// the daemon process); its `{"status", "body"}`.
    fn plugin_request<'a>(
        &'a self,
        profile: &'a str,
        id: &'a str,
        request: Value,
    ) -> HostFuture<'a, Value>;

    /// Applies a change to the profile; its JSON answer.
    fn change<'a>(
        &'a self,
        profile: &'a str,
        change: Change,
        target: Option<&'a str>,
        body: Value,
    ) -> HostFuture<'a, Value>;
}
