//! Subscriptions shared with tools (spec: model-providers, xAI SuperGrok
//! sign-in; Nous Portal subscription sign-in).
//!
//! A tool whose provider setting names a vendor (`xai`, `nous`) bills the
//! same subscription the vendor's models do: the vendor's sign-in
//! (`xai-oauth`, `nous`) wins over its API key unless the sign-in
//! provider's `prefer_credential = "api_key"`, and every call is labelled
//! with the `billing_source` of the credential it used.

use pmt_config::Config;
use pmt_config::schema::PreferCredential;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use serde::Serialize;

use crate::registry::ProviderRegistry;
use crate::resolve::{CredentialExtras, SecretSource};

/// The credential a tool call uses and how it bills.
#[derive(Debug, Clone)]
pub struct ToolCredential {
    /// The bearer token or key.
    pub token: SecretString,
    /// `subscription:<provider>/<label>` or `api_key:<provider>`.
    pub billing_source: String,
}

/// `(sign-in provider, API-key provider)` of a tool vendor.
#[must_use]
pub fn vendor_providers(vendor: &str) -> Option<(&'static str, &'static str)> {
    match vendor {
        "xai" => Some(("xai-oauth", "xai")),
        "nous" => Some(("nous", "nous")),
        _ => None,
    }
}

/// The credential of a tool whose provider setting is `vendor`: the
/// vendor's sign-in, else its API key (the key first when the sign-in
/// provider prefers `api_key`); `None` when neither exists.
///
/// # Errors
///
/// Unknown vendor providers, store errors.
pub fn tool_credential(
    registry: &ProviderRegistry,
    secrets: &dyn SecretSource,
    extras: &dyn CredentialExtras,
    vendor: &str,
) -> Result<Option<ToolCredential>, Error> {
    let Some((signin_id, key_id)) = vendor_providers(vendor) else {
        return Ok(None);
    };
    let signin = registry.lookup(signin_id)?;
    let keyed = registry.lookup(key_id)?;
    let subscription = extras
        .subscription(signin.id())?
        .filter(|s| !s.ineligible)
        .map(|s| ToolCredential {
            token: s.token,
            billing_source: format!("subscription:{}/{}", signin.id(), s.label),
        });
    let mut key = None;
    for name in keyed.secret_names() {
        if let Some(value) = secrets.secret(&name)? {
            key = Some(ToolCredential {
                token: value,
                billing_source: format!("api_key:{}", keyed.id()),
            });
            break;
        }
    }
    let key_first = signin.settings().prefer_credential == PreferCredential::ApiKey;
    Ok(if key_first {
        key.or(subscription)
    } else {
        subscription.or(key)
    })
}

/// One tool's routing to a vendor, as `prometheus auth status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolRoute {
    /// The tool.
    pub tool: &'static str,
    /// The setting that names its provider.
    pub setting: &'static str,
    /// Whether it names `vendor` now.
    pub routed: bool,
}

/// Which tools' provider settings name `vendor` (spec: model-providers,
/// Nous Portal subscription sign-in: the per-tool routing of `auth status
/// nous`).
#[must_use]
pub fn tool_routing(config: &Config, vendor: &str) -> Vec<ToolRoute> {
    let web = &config.web;
    let search = web.search_backend.as_deref().unwrap_or(&web.backend);
    let extract = web.extract_backend.as_deref().unwrap_or(&web.backend);
    let mut routes = vec![
        ToolRoute {
            tool: "web_search",
            setting: "web.search_backend",
            routed: search == vendor,
        },
        ToolRoute {
            tool: "web_extract",
            setting: "web.extract_backend",
            routed: extract == vendor,
        },
        ToolRoute {
            tool: "browser",
            setting: "browser.provider",
            routed: config.browser.provider == vendor,
        },
    ];
    if vendor == "xai" {
        routes.retain(|r| r.tool == "web_search");
    }
    routes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::resolve::SignedIn;

    struct SignedInto(&'static str);

    impl CredentialExtras for SignedInto {
        fn subscription(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
            Ok((provider == self.0).then(|| SignedIn {
                label: "personal".to_owned(),
                token: SecretString::new("sub-token"),
                account_id: None,
                ineligible: false,
                borrowed: None,
            }))
        }
    }

    fn keys(pairs: &[(&str, &str)]) -> BTreeMap<String, SecretString> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
            .collect()
    }

    #[test]
    fn tools_share_the_subscription() {
        let config = Config::default();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let used = tool_credential(&registry, &keys(&[]), &SignedInto("xai-oauth"), "xai")
            .unwrap()
            .unwrap();
        assert_eq!(used.token.expose(), "sub-token");
        assert_eq!(used.billing_source, "subscription:xai-oauth/personal");
        // The subscription wins over a key unless the sign-in prefers keys.
        let both = keys(&[("XAI_API_KEY", "xai-key")]);
        let used = tool_credential(&registry, &both, &SignedInto("xai-oauth"), "xai")
            .unwrap()
            .unwrap();
        assert_eq!(used.billing_source, "subscription:xai-oauth/personal");
        let prefers: Config =
            toml::from_str("[providers.xai-oauth]\nprefer_credential = \"api_key\"\n").unwrap();
        let registry = ProviderRegistry::for_profile(&prefers, Vec::new()).unwrap();
        let used = tool_credential(&registry, &both, &SignedInto("xai-oauth"), "xai")
            .unwrap()
            .unwrap();
        assert_eq!(used.billing_source, "api_key:xai");
    }

    #[test]
    fn nous_tools_follow_their_settings() {
        let config: Config = toml::from_str("[web]\nsearch_backend = \"nous\"\n").unwrap();
        let routes = tool_routing(&config, "nous");
        assert!(routes[0].routed);
        assert!(!routes[1].routed);
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let used = tool_credential(&registry, &keys(&[]), &SignedInto("nous"), "nous")
            .unwrap()
            .unwrap();
        assert_eq!(used.billing_source, "subscription:nous/personal");
    }
}
