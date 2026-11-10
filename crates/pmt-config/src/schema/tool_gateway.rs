//! The subscription tool gateway and the single provider selection of the
//! media tool categories (spec: model-providers, Subscription tool gateway
//! routing). The media tools' own settings join these sections when their
//! capabilities (web-and-media-tools, voice-mode) add them.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The `[tool_gateway]` section: where a subscription's tool gateway
/// answers (Nous by default; a self-hosted one by `domain`, `scheme` or
/// per-label `urls`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ToolGatewayConfig {
    /// Domain the gateways live under: `<label>-gateway.<domain>`.
    pub domain: String,
    /// URL scheme of the gateways (`https`; `http` only for a loopback or
    /// private self-hosted gateway).
    pub scheme: String,
    /// Per-label URL overrides (`web`, `image`, `video`, `tts`, `stt`,
    /// `browser`, `sandbox`, `connector`).
    pub urls: BTreeMap<String, String>,
    /// The secret holding a self-hosted gateway's user token; unset uses
    /// the subscription sign-in's token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<SecretName>,
}

impl Default for ToolGatewayConfig {
    fn default() -> Self {
        Self {
            domain: "nousresearch.com".to_owned(),
            scheme: "https".to_owned(),
            urls: BTreeMap::new(),
            token_secret: None,
        }
    }
}

impl ToolGatewayConfig {
    /// The gateway URL of `label`.
    #[must_use]
    pub fn url(&self, label: &str) -> String {
        self.urls
            .get(label)
            .cloned()
            .unwrap_or_else(|| format!("{}://{label}-gateway.{}", self.scheme, self.domain))
    }
}

/// A media tool category's provider selection (`[image_gen]`,
/// `[video_gen]`, `[tts]`, `[stt]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ToolProviderConfig {
    /// The provider serving the category (`nous` routes it through the
    /// subscription tool gateway); unset picks one automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_urls_follow_the_domain_unless_a_label_is_overridden() {
        let mut gateway = ToolGatewayConfig::default();
        assert_eq!(
            gateway.url("image"),
            "https://image-gateway.nousresearch.com"
        );
        gateway.domain = "tools.example.com".into();
        gateway
            .urls
            .insert("web".into(), "http://127.0.0.1:9/web".into());
        assert_eq!(
            gateway.url("image"),
            "https://image-gateway.tools.example.com"
        );
        assert_eq!(gateway.url("web"), "http://127.0.0.1:9/web");
    }
}
