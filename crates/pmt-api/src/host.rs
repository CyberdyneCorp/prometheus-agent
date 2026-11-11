//! What the API server needs from the daemon beyond the client protocol's
//! [`Host`]: the profile's store, configuration and key, API sessions,
//! turns with per-request model and approval choices, scheduled jobs and
//! the profile's catalogs. The daemon implements it; the HTTP layer never
//! reaches into sessions itself.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::UnattendedApproval;
use pmt_core::secret::SecretString;
use pmt_gateway::{Author, Host, HostFuture, Link};
use pmt_rpc::contracts::Submitted;
use pmt_store::Store;
use serde_json::Value;

/// The agent side of the API server.
pub trait ApiHost: Host {
    /// The profile's configuration now (`None`: not served).
    fn api_scope(&self, profile: &str) -> Option<ProfileScope>;

    /// The profile's store (`None`: not served).
    fn api_store(&self, profile: &str) -> Option<Arc<Store>>;

    /// The value of the profile's secret `name`.
    fn api_secret(&self, profile: &str, name: &str) -> Option<SecretString>;

    /// Creates an API session (`source = "api"`), seeded with `history`.
    fn api_create_session<'a>(
        &'a self,
        profile: &'a str,
        new: NewApiSession,
    ) -> HostFuture<'a, String>;

    /// Admits one API turn into its session (queued behind a running one).
    fn api_submit<'a>(
        &'a self,
        profile: &'a str,
        turn: ApiTurn,
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted>;

    /// Checks a requested route: `Ok(false)` when the session's own
    /// `/model` choice wins over it.
    ///
    /// An unknown provider is an error naming it.
    fn api_check_route<'a>(
        &'a self,
        profile: &'a str,
        session_id: Option<&'a str>,
        route: &'a ModelChoice,
    ) -> HostFuture<'a, bool>;

    /// A Jobs REST call; the HTTP status and body.
    fn api_jobs<'a>(&'a self, profile: &'a str, call: JobsCall) -> HostFuture<'a, (u16, Value)>;

    /// A catalog of the profile.
    fn api_catalog<'a>(&'a self, profile: &'a str, what: Catalog) -> HostFuture<'a, Value>;

    /// The profile's bounded readiness checks (never values, credentials,
    /// paths or raw errors).
    fn api_health<'a>(&'a self, profile: &'a str) -> HostFuture<'a, Value>;

    /// `text` with the profile's known secrets and every credential-shaped
    /// token redacted (tool result previews).
    fn api_redact(&self, profile: &str, text: &str) -> String {
        let _ = profile;
        pmt_core::secret::Redactor::new(std::iter::empty()).redact_all(text)
    }
}

/// A new API session.
#[derive(Debug, Clone, Default)]
pub struct NewApiSession {
    /// Caller instructions, layered after the profile's system prompt.
    pub instructions: Option<String>,
    /// The caller's `X-Prometheus-Session-Key`.
    pub session_key: Option<String>,
    /// The Chat Completions conversation hash.
    pub derived_key: Option<String>,
    /// Earlier turns the client sent: `(role, text)` with `user` or
    /// `assistant` roles.
    pub history: Vec<(String, String)>,
    /// A title.
    pub title: Option<String>,
}

/// One API turn.
#[derive(Debug, Clone)]
pub struct ApiTurn {
    /// The session.
    pub session_id: String,
    /// The user message.
    pub text: String,
    /// Its images.
    pub images: Vec<ApiImage>,
    /// Notices for this turn only (changed caller instructions).
    pub additions: Vec<String>,
    /// The route this turn's model calls take, when the request picked
    /// one (the session's prompt and tools stay as they are).
    pub model: Option<ModelChoice>,
    /// Request-scoped model options.
    pub options: ModelOptions,
    /// Decide approvals at once this way (no answer channel); `None`
    /// asks the session's clients.
    pub unattended: Option<UnattendedApproval>,
}

/// An image of a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiImage {
    /// A `data:image/*` URL's bytes and media type.
    Data {
        /// `image/png`, ...
        media_type: String,
        /// The decoded bytes.
        bytes: Vec<u8>,
    },
    /// A remote `http(s)` URL the daemon fetches (SSRF-checked).
    Url(String),
}

/// A requested provider and model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// Provider id.
    pub provider: String,
    /// Model at the provider; `None` is its default.
    pub model: Option<String>,
}

/// Request-scoped model options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOptions {
    /// `reasoning_effort` (`none`, `low`, `medium`, `high`, ...).
    pub reasoning_effort: Option<String>,
    /// `service_tier` (`priority` asks for fast mode).
    pub service_tier: Option<String>,
}

/// A Jobs REST request.
#[derive(Debug, Clone)]
pub struct JobsCall {
    /// `GET`, `POST`, `PATCH`, `DELETE`.
    pub method: String,
    /// The job id, when the path names one.
    pub job: Option<String>,
    /// `pause`, `resume` or `run`.
    pub action: Option<String>,
    /// The JSON body (`{}` when none).
    pub body: Value,
}

/// A profile catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    /// `GET /v1/skills`.
    Skills,
    /// `GET /v1/toolsets`.
    Toolsets,
    /// `GET /api/model/options`: the configured providers and models with
    /// capability hints and pricing.
    Models,
}
