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
    pub plan_notes: String,
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
            plan_notes: String::new(),
            secret_names: Vec::new(),
            key_hosts,
            models_url: None,
            supports_model_listing: true,
            supports_prompt_cache: false,
            hidden: false,
            local: false,
            default_model: None,
            allow_insecure_http: false,
            host_rules: Vec::new(),
            wire_need: WireNeed::Native,
            endpoint_note: None,
            keyless_on_loopback: false,
        }
    }

    /// The wire format as listings show it: the native format, or
    /// `per-model`, `auto-detected`, or the plugin/virtual format.
    #[must_use]
    pub fn wire_label(&self) -> &str {
        match &self.wire_need {
            WireNeed::Native => self.wire_format.as_str(),
            WireNeed::PerModel => "per-model",
            WireNeed::Detected => "auto-detected",
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
    fn anthropic_path_rule() {
        let rule = &default_host_rules()[0];
        assert!(rule.matches("https://api.minimax.io/anthropic"));
        assert!(rule.matches("https://relay.test/anthropic/v1/"));
        assert!(!rule.matches("https://relay.test/v1"));
    }
}
