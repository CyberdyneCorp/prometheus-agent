//! The `[api_server]` section (spec: api-server): what a profile offers
//! on the daemon's OpenAI-compatible HTTP listener. The listener itself
//! (address, public exposure, browser origins) is machine-wide and lives
//! in `daemon.toml`; every key here is read per request.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{SecretName, UnattendedApproval};

/// The `[api_server]` section.
#[allow(clippy::struct_excessive_bools)] // independent switches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct ApiServerConfig {
    /// Serve this profile on the daemon's API listener (`/p/<profile>/`,
    /// and without prefix for `default`). It also needs the key named by
    /// `key_secret`.
    #[schemars(extend("x-apply" = "restart"))]
    pub enabled: bool,
    /// The secret holding this profile's bearer key (at least 32
    /// characters); without it the profile's routes answer 404.
    pub key_secret: SecretName,
    /// The model id `/v1/models` advertises (default: the profile name,
    /// `prometheus-agent` for `default`).
    pub model_name: Option<String>,
    /// Extra model ids `/v1/models` advertises, each a route a request
    /// can pick by sending its name as `model`.
    pub model_routes: BTreeMap<String, ApiModelRoute>,
    /// Honour a bare `model` without `provider` on Chat Completions and
    /// Responses (generic clients send `gpt-4o`; off keeps the profile's
    /// default).
    pub direct_model_requests: bool,
    /// Toolsets of API sessions instead of the `api_server` default;
    /// toolsets the profile does not enable are dropped.
    pub toolsets: Option<Vec<String>>,
    /// The decision for approvals on Chat Completions and Responses,
    /// which have no answer channel (Runs and session chat ask).
    pub unattended_approvals: UnattendedApproval,
    /// Stream tool starts as `prometheus.tool.progress` SSE events.
    pub tool_progress_events: bool,
    /// Runs executing at once for this profile; `0` disables the cap.
    #[schemars(range(max = 10_000))]
    pub max_concurrent_runs: u32,
    /// The Responses API store.
    pub responses: ApiResponsesConfig,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key_secret: SecretName("API_SERVER_KEY".to_owned()),
            model_name: None,
            model_routes: BTreeMap::new(),
            direct_model_requests: false,
            toolsets: None,
            unattended_approvals: UnattendedApproval::Deny,
            tool_progress_events: true,
            max_concurrent_runs: 10,
            responses: ApiResponsesConfig::default(),
        }
    }
}

/// A route alias of `[api_server.model_routes]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApiModelRoute {
    /// The provider id.
    pub provider: String,
    /// The model at the provider (default: the provider's default).
    #[serde(default)]
    pub model: Option<String>,
}

/// `[api_server.responses]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApiResponsesConfig {
    /// Stored responses kept; the least recently used go first.
    #[schemars(range(min = 1, max = 100_000))]
    pub max_stored: u32,
    /// Characters kept of each stored tool output (its head plus
    /// `...[<n> more chars]`); `0` keeps them verbatim.
    pub history_tool_output_max_chars: u64,
}

impl Default for ApiResponsesConfig {
    fn default() -> Self {
        Self {
            max_stored: 100,
            history_tool_output_max_chars: 0,
        }
    }
}

impl ApiServerConfig {
    /// The model id this profile advertises.
    #[must_use]
    pub fn model_id(&self, profile: &str) -> String {
        match &self.model_name {
            Some(name) => name.clone(),
            None if profile == "default" => "prometheus-agent".to_owned(),
            None => profile.to_owned(),
        }
    }
}
