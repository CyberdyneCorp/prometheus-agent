//! Model, provider and auxiliary routing sections (spec: model-providers).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The `[model]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ModelConfig {
    /// Provider id or alias; unset means `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Default model id (or a `[models]` alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Provider order tried by `auto` among providers with a stored secret.
    pub auto_order: Vec<String>,
    /// Smallest context window (tokens) a session may start with.
    #[schemars(range(min = 16_000))]
    pub min_context_window: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: None,
            default: None,
            auto_order: ["anthropic", "openai", "openrouter", "google", "nous"]
                .map(str::to_owned)
                .to_vec(),
            min_context_window: 64_000,
        }
    }
}

/// One `[providers.<id>]` override of a built-in or custom provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// `false` makes every request to this provider fail with
    /// `provider.disabled`.
    pub enabled: bool,
    /// API base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Name of the secret holding this provider's key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_secret: Option<SecretName>,
    /// Hostnames the provider's secret may be sent to (exact, or a leading
    /// `*.` wildcard).
    pub key_hosts: Vec<String>,
    /// Which credential wins when both exist.
    pub prefer_credential: PreferCredential,
    /// Credential pool selection.
    pub pool_strategy: PoolStrategy,
    /// Same-route retries of a retryable failure.
    #[schemars(range(min = 0, max = 6))]
    pub max_retries: u32,
    /// Per-request timeout.
    pub request_timeout_secs: u64,
    /// Longest gap between stream events.
    pub stream_idle_timeout_secs: u64,
    /// Lifetime of the cached live model listing.
    pub catalog_ttl_secs: u64,
    /// Sign-in flow for OAuth providers; unset uses the provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_flow: Option<LoginFlow>,
    /// Eligibility and billing notes shown before sign-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_notes: Option<String>,
    /// Script file of the built-in `mock` provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: None,
            key_secret: None,
            key_hosts: Vec::new(),
            prefer_credential: PreferCredential::Subscription,
            pool_strategy: PoolStrategy::FillFirst,
            max_retries: 2,
            request_timeout_secs: 600,
            stream_idle_timeout_secs: 120,
            catalog_ttl_secs: 86_400,
            login_flow: None,
            plan_notes: None,
            script: None,
        }
    }
}

/// Credential preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreferCredential {
    /// A subscription sign-in.
    Subscription,
    /// An API key.
    ApiKey,
}

/// Credential pool strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PoolStrategy {
    /// Use the first healthy credential until it is benched.
    FillFirst,
    /// Rotate per request.
    RoundRobin,
    /// Pick at random.
    Random,
    /// Pick the least used.
    LeastUsed,
}

/// OAuth sign-in flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginFlow {
    /// Device code.
    Device,
    /// Browser with PKCE.
    Browser,
}

/// One `[models]` entry: an alias string `"<provider>/<model>"` or a route
/// and metadata table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ModelEntry {
    /// `"<provider>/<model>"`.
    Alias(String),
    /// A route with optional metadata overrides.
    Route(ModelRoute),
}

/// A `[models.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRoute {
    /// Provider id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id at the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Base URL override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Name of the secret for this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_secret: Option<SecretName>,
    /// Context window override in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Output token cap override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Prices per million tokens (USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<ModelPrice>,
}

/// Per-million-token prices in USD.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPrice {
    /// Input tokens.
    #[schemars(range(min = 0.0))]
    pub input: f64,
    /// Output tokens.
    #[schemars(range(min = 0.0))]
    pub output: f64,
    /// Cache-read tokens.
    #[schemars(range(min = 0.0))]
    pub cache_read: f64,
    /// Cache-write tokens.
    #[schemars(range(min = 0.0))]
    pub cache_write: f64,
}

/// The `[auxiliary]` section: one optional table per auxiliary task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct AuxiliaryConfig {
    /// Same-route retries of a retryable auxiliary failure.
    #[schemars(range(min = 0, max = 6))]
    pub transient_retries: u32,
    /// Context compression summaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<AuxiliaryTask>,
    /// Session titles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<AuxiliaryTask>,
    /// Image descriptions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<AuxiliaryTask>,
    /// Smart approvals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<AuxiliaryTask>,
    /// Memory review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<AuxiliaryTask>,
    /// Skill curator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curator: Option<AuxiliaryTask>,
    /// Web extraction summaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_extract: Option<AuxiliaryTask>,
    /// Skill review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_review: Option<AuxiliaryTask>,
    /// Harness evolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evolution: Option<AuxiliaryTask>,
    /// Goal judging (defaults to the main model).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_judge: Option<AuxiliaryTask>,
    /// Side questions (defaults to the main model).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_question: Option<AuxiliaryTask>,
    /// Voice chat (defaults to the main model).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_chat: Option<AuxiliaryTask>,
}

impl Default for AuxiliaryConfig {
    fn default() -> Self {
        Self {
            transient_retries: 2,
            compression: None,
            title: None,
            vision: None,
            approval: None,
            memory: None,
            curator: None,
            web_extract: None,
            skill_review: None,
            evolution: None,
            goal_judge: None,
            side_question: None,
            voice_chat: None,
        }
    }
}

/// One `[auxiliary.<task>]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuxiliaryTask {
    /// Provider id; unset means `auto` (the session's main route).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Timeout; unset uses the task default (120 s for `vision` and
    /// `compression`, 30 s for `approval` and `title`, 600 s for `curator`
    /// and `evolution`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Fallback routes (`"<provider>/<model>"`); `[]` disables fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_chain: Option<Vec<String>>,
}

/// The `[fallback]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct FallbackConfig {
    /// Fallback entries with a health score below this are skipped.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub min_health: f64,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self { min_health: 0.2 }
    }
}

/// The `[auth]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct AuthConfig {
    /// Use an existing Claude Code or Codex CLI sign-in when the profile
    /// holds no credential of its own.
    pub adopt_external_logins: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            adopt_external_logins: true,
        }
    }
}
