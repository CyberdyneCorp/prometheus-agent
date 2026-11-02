//! `[gateway.platforms.webhook.routes.<name>]` (spec: platform-adapters,
//! Webhook routes; Webhook limits, idempotency and responses; Webhook
//! payload filters and transform scripts; Webhook direct delivery mode):
//! what a route admits, how its payload becomes a turn's input or a
//! forwarded message, and how hard it may be hit.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SecretName;

/// One webhook route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookRoute {
    /// Event types admitted (`X-GitHub-Event`, `X-GitLab-Event` or the
    /// payload's `event_type`); empty admits every event.
    pub events: Vec<String>,
    /// The secret every request must be signed with (GitHub, GitLab,
    /// Standard Webhooks or the generic scheme).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_secret: Option<SecretName>,
    /// Admit unsigned requests (only while `gateway.http.listen` is a
    /// loopback address).
    pub insecure_no_auth: bool,
    /// The turn's input: `{a.b.c}` paths into the payload, `{__raw__}`
    /// the whole payload. Unset: the payload as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Skills preloaded into each delivery's session (ignored with
    /// `deliver_only`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Toolsets of each delivery's session, replacing the default webhook
    /// toolset rather than adding to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<String>>,
    /// Where the answer goes: `log` (recorded under
    /// `<profile>/logs/webhook/<route>.jsonl`), `origin`, `github_comment`
    /// or a send target `platform[:chat_id[:thread_id]]`. Unset: `log`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,
    /// More send targets that receive a copy.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deliver_extra: Vec<String>,
    /// Run no model: the rendered prompt is sent verbatim to `deliver`.
    pub deliver_only: bool,
    /// Requests per minute the route admits (fixed window); more are
    /// answered 429.
    #[schemars(range(min = 1, max = 100_000))]
    pub rate_limit_per_min: u32,
    /// Conditions a payload must meet; a request that fails them is
    /// answered `ignored` with reason `filter`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<WebhookFilters>,
    /// A transform script under the profile's `scripts/` directory, run in
    /// the sandbox with the payload JSON on stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Seconds the transform script may run.
    #[schemars(range(min = 1, max = 3600))]
    pub script_timeout_secs: u64,
}

impl Default for WebhookRoute {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            secret_secret: None,
            insecure_no_auth: false,
            prompt: None,
            skills: Vec::new(),
            toolsets: None,
            deliver: None,
            deliver_extra: Vec::new(),
            deliver_only: false,
            rate_limit_per_min: 30,
            filters: None,
            script: None,
            script_timeout_secs: 30,
        }
    }
}

impl WebhookRoute {
    /// The delivery target in effect (`log` when unset).
    #[must_use]
    pub fn deliver_target(&self) -> &str {
        self.deliver.as_deref().unwrap_or("log")
    }
}

/// A route's filters: one condition or a list that must all hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum WebhookFilters {
    /// One condition.
    One(WebhookFilter),
    /// Every condition.
    All(Vec<WebhookFilter>),
}

impl WebhookFilters {
    /// The conditions, all of which must hold.
    #[must_use]
    pub fn conditions(&self) -> &[WebhookFilter] {
        match self {
            Self::One(one) => std::slice::from_ref(one),
            Self::All(all) => all,
        }
    }
}

/// One filter condition over `payload.<path>`, `event` or
/// `headers.<Name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WebhookFilter {
    /// The path resolves.
    Exists(FilterPath),
    /// The path does not resolve.
    Missing(FilterPath),
    /// The value equals `value`.
    Equals(FilterValue),
    /// The value differs from `value` (or is missing).
    NotEquals(FilterValue),
    /// The string contains `value`, or the list holds it.
    Contains(FilterValue),
    /// The value is one of `values`.
    In(FilterValues),
    /// The value is a line of `file` (relative to the profile directory).
    InFile(FilterFile),
    /// The string matches the regular expression `pattern`.
    Regex(FilterPattern),
    /// Every condition holds.
    All(Vec<WebhookFilter>),
    /// At least one condition holds.
    Any(Vec<WebhookFilter>),
    /// The condition does not hold.
    Not(Box<WebhookFilter>),
}

/// `{path}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterPath {
    /// `payload.<a.b>`, `event` or `headers.<Name>`.
    pub path: String,
}

/// `{path, value}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterValue {
    /// `payload.<a.b>`, `event` or `headers.<Name>`.
    pub path: String,
    /// The value compared.
    pub value: Value,
}

/// `{path, values}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterValues {
    /// `payload.<a.b>`, `event` or `headers.<Name>`.
    pub path: String,
    /// The accepted values.
    pub values: Vec<Value>,
}

/// `{path, file}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterFile {
    /// `payload.<a.b>`, `event` or `headers.<Name>`.
    pub path: String,
    /// A file of accepted values, one per line, relative to the profile
    /// directory.
    pub file: String,
}

/// `{path, pattern}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterPattern {
    /// `payload.<a.b>`, `event` or `headers.<Name>`.
    pub path: String,
    /// A regular expression.
    pub pattern: String,
}

impl WebhookFilter {
    /// Every condition of this tree with a path, depth first.
    pub fn visit(&self, f: &mut dyn FnMut(&Self)) {
        f(self);
        match self {
            Self::All(list) | Self::Any(list) => list.iter().for_each(|c| c.visit(f)),
            Self::Not(inner) => inner.visit(f),
            _ => {}
        }
    }

    /// The path the condition reads, for leaf conditions.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Exists(p) | Self::Missing(p) => Some(&p.path),
            Self::Equals(v) | Self::NotEquals(v) | Self::Contains(v) => Some(&v.path),
            Self::In(v) => Some(&v.path),
            Self::InFile(f) => Some(&f.path),
            Self::Regex(p) => Some(&p.path),
            Self::All(_) | Self::Any(_) | Self::Not(_) => None,
        }
    }
}
