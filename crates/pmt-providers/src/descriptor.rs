//! The provider descriptor contract (spec: model-providers, Provider
//! descriptor contract): what every provider (built-in, `[providers.<id>]`
//! block or plugin) declares, and the checks a descriptor must pass before
//! it enters a registry.

use pmt_config::provider_urls::{self, UrlProblem};
use pmt_config::schema::{LoginFlow, ProviderAuth, WireFormat};
use pmt_core::error::{Error, codes};

/// A rule that forces a wire format for base URLs matching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    /// Exact host the rule applies to; `None` matches every host.
    pub host: Option<String>,
    /// Path suffixes (without trailing slash) that select `wire_format`.
    pub path_suffixes: Vec<String>,
    /// The format the host mandates.
    pub wire_format: WireFormat,
}

impl HostRule {
    /// Whether `base_url` matches this rule.
    #[must_use]
    pub fn matches(&self, base_url: &str) -> bool {
        let Some(host) = provider_urls::host_of(base_url) else {
            return false;
        };
        if self
            .host
            .as_deref()
            .is_some_and(|h| !h.eq_ignore_ascii_case(&host))
        {
            return false;
        }
        let path = url_path(base_url);
        self.path_suffixes.is_empty()
            || self
                .path_suffixes
                .iter()
                .any(|s| path.ends_with(s.as_str()))
    }
}

/// The path of an `http(s)` URL without query and trailing slash.
fn url_path(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    path.split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
}

/// Rules every descriptor carries after its own: a base URL path ending
/// `/anthropic` or `/anthropic/v1` speaks Anthropic Messages.
#[must_use]
pub fn default_host_rules() -> Vec<HostRule> {
    vec![HostRule {
        host: None,
        path_suffixes: vec!["/anthropic".to_owned(), "/anthropic/v1".to_owned()],
        wire_format: WireFormat::AnthropicMessages,
    }]
}

/// What a descriptor's wire format needs beyond the three native adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireNeed {
    /// A native adapter: `wire_format`.
    Native,
    /// The format varies per model (`model_wire_formats`); `wire_format`
    /// is used until a model maps elsewhere.
    PerModel,
    /// Detected from the endpoint's deployment metadata; `wire_format`
    /// until detected.
    Detected,
    /// A non-native format only a provider plugin implements. Routing
    /// fails with `provider.plugin_unavailable` unless config or a host
    /// rule selects a native format.
    Plugin {
        /// The format (`gemini_native`, `bedrock_converse`...).
        wire: String,
        /// The plugin that implements it.
        plugin: String,
        /// What the user can do meanwhile.
        hint: String,
    },
    /// The built-in deterministic `mock` provider, served in-process from
    /// a script (no network, no credential).
    Mock,
    /// A virtual provider served in-process by another capability.
    Virtual {
        /// The listed wire format (`aggregator's`).
        wire: String,
        /// The owning capability (`mixture-of-agents`).
        owner: String,
    },
}

/// One provider's static description. The flags are the descriptor
/// contract's own fields, hence the bools.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Canonical id.
    pub id: String,
    /// Other names the provider answers to.
    pub aliases: Vec<String>,
    /// Declared wire format.
    pub wire_format: WireFormat,
    /// Default endpoint; `None` when the user must configure one.
    pub base_url: Option<String>,
    /// Credential kind.
    pub auth: ProviderAuth,
    /// Sign-in flows (required when `auth` is `oauth`).
    pub login_flows: Vec<LoginFlow>,
    /// Eligibility and billing notes shown before sign-in.
    pub plan_notes: Vec<String>,
    /// Secret names, in resolution order (plan-specific before shared).
    pub secret_names: Vec<String>,
    /// Hosts the provider's secrets may be sent to (exact, or `*.` prefix).
    pub key_hosts: Vec<String>,
    /// Model listing URL; `None` is `<base_url>/models`.
    pub models_url: Option<String>,
    /// Whether the provider lists its models.
    pub supports_model_listing: bool,
    /// Whether the provider honours a prompt cache key.
    pub supports_prompt_cache: bool,
    /// Whether the provider compacts context server-side on the Responses
    /// wire (a `context_management` compaction setting answered with an
    /// opaque compaction item).
    pub supports_server_compaction: bool,
    /// Hidden from pickers and listings.
    pub hidden: bool,
    /// A local inference server: the default endpoint is probed and the
    /// provider never falls back to a remote one.
    pub local: bool,
    /// Model used when none is requested.
    pub default_model: Option<String>,
    /// Plain HTTP to public hosts allowed.
    pub allow_insecure_http: bool,
    /// Host rules checked before the declared format.
    pub host_rules: Vec<HostRule>,
    /// What the wire format needs beyond the native adapters.
    pub wire_need: WireNeed,
    /// How the endpoint is found when there is no default `base_url`
    /// (listings show it).
    pub endpoint_note: Option<String>,
    /// No credential is needed when the configured endpoint is loopback
    /// (a hosted service with a local daemon).
    pub keyless_on_loopback: bool,
    /// The OAuth PKCE sign-in a provider plugin declares (spec:
    /// model-providers, Plugin OAuth and external-process providers): the
    /// host runs it, so the plugin never sees the verifier or `state`.
    pub oauth: Option<PluginOAuth>,
    /// The agent CLI an `external_process` provider suggests. It is only
    /// named in the hint: the host launches nothing a plugin chose, only
    /// the `providers.<id>.command` the user configured.
    pub command: Option<String>,
    /// An opaque per-conversation header (spec: model-providers, Session
    /// affinity headers and per-model wire routing) whose value is a
    /// stable hash of the conversation, sent on main and auxiliary calls.
    pub session_affinity_header: Option<String>,
    /// Model id patterns mapped to the wire format serving them, first
    /// match wins; models matching none use `wire_format`.
    pub model_wire_formats: Vec<ModelWire>,
    /// Where a subscription's plan and billing are managed (`/subscription`,
    /// `/topup`).
    pub account_urls: AccountUrls,
}

/// A subscription provider's account pages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountUrls {
    /// Plan management (upgrade, downgrade).
    pub plan: Option<String>,
    /// Billing and prepaid balance top-up.
    pub billing: Option<String>,
}

/// One `model_wire_formats` row: a case-insensitive model id pattern
/// (`*` matches any run of characters) and its wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWire {
    /// The pattern (`minimax*`, `claude-*`).
    pub pattern: String,
    /// The format of matching models.
    pub wire_format: WireFormat,
}

impl ModelWire {
    /// Whether `model` matches the pattern.
    #[must_use]
    pub fn matches(&self, model: &str) -> bool {
        glob_matches(
            &self.pattern.to_ascii_lowercase(),
            &model.to_ascii_lowercase(),
        )
    }
}

/// `*`-only glob matching.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.ends_with(last)
}

/// A plugin's OAuth PKCE sign-in: a public client, its endpoints and the
/// loopback redirect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginOAuth {
    /// The public client id (no secret: PKCE is the proof).
    pub client_id: String,
    /// Browser authorization endpoint.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Hosts the endpoints may be on; empty means the authorize URL's
    /// host and its subdomains.
    pub allowed_hosts: Vec<String>,
    /// Fixed redirect port, or 0 for an OS-assigned one.
    pub redirect_port: u16,
    /// Redirect path (`/callback` when unset).
    pub redirect_path: Option<String>,
    /// Extra authorization request parameters.
    pub authorize_params: Vec<(String, String)>,
}

impl Descriptor {
    /// A descriptor with every optional field at its contract default.
    #[must_use]
    pub fn new(id: &str, wire_format: WireFormat, base_url: Option<&str>) -> Self {
        let key_hosts = base_url
            .and_then(provider_urls::host_of)
            .into_iter()
            .collect();
        Self {
            id: id.to_owned(),
            aliases: Vec::new(),
            wire_format,
            base_url: base_url.map(str::to_owned),
            auth: ProviderAuth::ApiKey,
            login_flows: Vec::new(),
            plan_notes: Vec::new(),
            secret_names: Vec::new(),
            key_hosts,
            models_url: None,
            supports_model_listing: true,
            supports_prompt_cache: false,
            supports_server_compaction: false,
            hidden: false,
            local: false,
            default_model: None,
            allow_insecure_http: false,
            host_rules: Vec::new(),
            wire_need: WireNeed::Native,
            endpoint_note: None,
            keyless_on_loopback: false,
            oauth: None,
            command: None,
            session_affinity_header: None,
            model_wire_formats: Vec::new(),
            account_urls: AccountUrls::default(),
        }
    }

    /// The wire format `model_wire_formats` maps `model` to, if any.
    #[must_use]
    pub fn model_wire(&self, model: &str) -> Option<WireFormat> {
        self.model_wire_formats
            .iter()
            .find(|row| row.matches(model))
            .map(|row| row.wire_format)
    }

    /// The wire format as listings show it: the native format, or
    /// `per-model`, `auto-detected`, or the plugin/virtual format.
    #[must_use]
    pub fn wire_label(&self) -> &str {
        match &self.wire_need {
            WireNeed::Native => self.wire_format.as_str(),
            WireNeed::PerModel => "per-model",
            WireNeed::Detected => "auto-detected",
            WireNeed::Mock => "mock_script",
            WireNeed::Plugin { wire, .. } | WireNeed::Virtual { wire, .. } => wire,
        }
    }

    /// The model listing URL for `base_url`.
    #[must_use]
    pub fn models_url_for(&self, base_url: &str) -> String {
        self.models_url
            .clone()
            .unwrap_or_else(|| format!("{}/models", base_url.trim_end_matches('/')))
    }

    /// Checks the contract: id shape, sign-in flows for OAuth, and the
    /// base URL transport rule.
    ///
    /// # Errors
    ///
    /// `provider.invalid_descriptor` naming the field.
    pub fn validate(&self) -> Result<(), Error> {
        if !is_provider_id(&self.id) {
            return Err(invalid(&format!(
                "provider id '{}' must match ^[a-z0-9][a-z0-9._-]*$",
                self.id
            )));
        }
        if let Some(alias) = self.aliases.iter().find(|a| !is_provider_id(a)) {
            return Err(invalid(&format!(
                "providers.{}.aliases: '{alias}' is not a valid provider name",
                self.id
            )));
        }
        if self.auth == ProviderAuth::Oauth && self.login_flows.is_empty() {
            return Err(invalid(&format!(
                "providers.{}.login_flows is required when auth = \"oauth\" (device, browser)",
                self.id
            )));
        }
        if let Some(url) = &self.base_url {
            let key = format!("providers.{}.base_url", self.id);
            provider_urls::check_url(url, self.allow_insecure_http).map_err(|p| match p {
                UrlProblem::Malformed => invalid(&format!("{key}: '{url}' is not an http(s) URL")),
                problem @ UrlProblem::InsecurePublic { .. } => {
                    provider_urls::problem_error(&key, &self.id, url, &problem)
                }
            })?;
        }
        Ok(())
    }
}

/// `^[a-z0-9][a-z0-9._-]*$`.
#[must_use]
pub fn is_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

fn invalid(message: &str) -> Error {
    Error::new(&codes::PROVIDER_INVALID_DESCRIPTOR, message)
}

/// Whether `host` is matched by a `key_hosts` entry: exact
/// (case-insensitive) or a leading `*.` wildcard covering subdomains only.
#[must_use]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|label| label.len() > 1 && label.ends_with('.')),
        None => host == pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_without_login_flows_is_refused() {
        let mut d = Descriptor::new("acme", WireFormat::ChatCompletions, Some("https://a.test"));
        d.auth = ProviderAuth::Oauth;
        let err = d.validate().unwrap_err();
        assert_eq!(err.code().code, "provider.invalid_descriptor");
        assert!(err.message().contains("login_flows"));
        d.login_flows = vec![LoginFlow::Device];
        d.validate().unwrap();
    }

    #[test]
    fn plain_http_public_base_url_needs_the_opt_in() {
        let mut d = Descriptor::new(
            "p",
            WireFormat::ChatCompletions,
            Some("http://llm.example.com/v1"),
        );
        assert!(
            d.validate()
                .unwrap_err()
                .message()
                .contains("allow_insecure_http")
        );
        d.allow_insecure_http = true;
        d.validate().unwrap();
        let local = Descriptor::new(
            "q",
            WireFormat::ChatCompletions,
            Some("http://127.0.0.1:11434/v1"),
        );
        local.validate().unwrap();
    }

    #[test]
    fn key_hosts_match_exactly_or_by_explicit_wildcard() {
        assert!(host_matches("api.deepseek.com", "API.deepseek.com"));
        assert!(!host_matches(
            "api.deepseek.com",
            "api.deepseek.com.attacker.test"
        ));
        assert!(!host_matches("deepseek.com", "api.deepseek.com"));
        assert!(host_matches("*.example.com", "a.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("*.example.com", "badexample.com"));
    }

    #[test]
    fn model_wire_patterns_match_case_insensitively() {
        let row = ModelWire {
            pattern: "minimax*".to_owned(),
            wire_format: WireFormat::AnthropicMessages,
        };
        assert!(row.matches("MiniMax-M2.5"));
        assert!(!row.matches("glm-5"));
        assert!(glob_matches("claude-*-vision", "claude-x-vision"));
        assert!(!glob_matches("claude-*-vision", "claude-x"));
        assert!(glob_matches("exact", "exact"));
        assert!(!glob_matches("exact", "exactly"));
    }

    #[test]
    fn anthropic_path_rule() {
        let rule = &default_host_rules()[0];
        assert!(rule.matches("https://api.minimax.io/anthropic"));
        assert!(rule.matches("https://relay.test/anthropic/v1/"));
        assert!(!rule.matches("https://relay.test/v1"));
    }
}
