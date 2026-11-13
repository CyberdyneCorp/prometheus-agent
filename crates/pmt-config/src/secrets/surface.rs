//! Surface-only secrets (spec: security-boundaries, Gateway-owned secrets
//! are never delegable).
//!
//! The credentials a surface owns (messaging bot tokens, webhook signing
//! keys, proxy URLs with passwords, the relay credential) are read by that
//! surface alone. No tool, child process, skill or plugin can be granted
//! one: every place that hands a secret onwards asks [`SurfaceOnly`] first
//! and refuses with `secret '<NAME>' is surface-only`.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// The default secret names of the built-in surfaces (each adapter's own
/// constant names the same string; the binary's tests check they agree).
pub const BUILTIN: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "DISCORD_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "SLACK_APP_TOKEN",
    "MATRIX_ACCESS_TOKEN",
    "EMAIL_PASSWORD",
    "WHATSAPP_BRIDGE_TOKEN",
    crate::daemon_relay::DEFAULT_TOKEN_SECRET,
    crate::daemon_dashboard::PASSWORD_HASH_SECRET,
];

/// The surface-only secret names of one profile, each with the platform
/// that owns it (`None`: a built-in surface, never a plugin).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceOnly {
    names: BTreeMap<String, Option<String>>,
}

impl SurfaceOnly {
    /// The built-in names plus every secret the profile's gateway
    /// configuration names: platform tokens and proxy URLs, adapter
    /// options ending in `_secret`, webhook route secrets.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        let mut names: BTreeMap<String, Option<String>> =
            BUILTIN.iter().map(|n| ((*n).to_owned(), None)).collect();
        let gateway = &config.gateway;
        if let Some(name) = &gateway.proxy_url_secret {
            names.insert(name.0.clone(), None);
        }
        for (platform_name, platform) in &gateway.platforms {
            // A first-party platform's adapter is host code: no plugin owns
            // its credentials.
            let owner = crate::platforms::first_party(platform_name)
                .is_none()
                .then(|| platform_name.clone());
            let mut add = |name: &str| {
                names
                    .entry(name.to_owned())
                    .or_insert_with(|| owner.clone());
            };
            for name in platform
                .token_secret
                .iter()
                .chain(&platform.proxy_url_secret)
            {
                add(&name.0);
            }
            for (key, value) in &platform.options {
                if key.ends_with("_secret")
                    && let Some(name) = value.as_str()
                {
                    add(name);
                }
            }
            for route in platform.routes.values() {
                for name in route.secret_secret.iter().chain(&route.github_token_secret) {
                    add(&name.0);
                }
            }
        }
        Self { names }
    }

    /// Whether `name` is surface-only.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    /// The plugin platform whose adapter owns `name`, when a plugin adapter
    /// does (a built-in surface's secret has none).
    #[must_use]
    pub fn plugin_owner(&self, name: &str) -> Option<&str> {
        self.names.get(name).and_then(Option::as_deref)
    }

    /// Refuses a surface-only `name`.
    ///
    /// # Errors
    ///
    /// `secret.surface_only`: `secret '<NAME>' is surface-only`.
    pub fn check(&self, name: &str) -> Result<(), Error> {
        if self.contains(name) {
            return Err(refusal(name));
        }
        Ok(())
    }

    /// Every name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }
}

/// The refusal for surface-only secret `name`.
#[must_use]
pub fn refusal(name: &str) -> Error {
    Error::new(
        &codes::SECRET_SURFACE_ONLY,
        format!("secret '{name}' is surface-only"),
    )
    .with_hint("a surface's credential is never delegated; store a separate secret for the tool, skill or plugin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_tokens_and_configured_surface_secrets_are_surface_only() {
        let mut config = Config::default();
        let platform = crate::schema::PlatformConfig {
            token_secret: Some(crate::schema::SecretName("MY_BOT".into())),
            options: [("signing_secret".to_owned(), serde_json::json!("SIGNING"))].into(),
            ..Default::default()
        };
        config.gateway.platforms.insert("slack".into(), platform);
        let only = SurfaceOnly::of(&config);
        for name in ["TELEGRAM_BOT_TOKEN", "MY_BOT", "SIGNING", "PMT_RELAY_TOKEN"] {
            assert_eq!(
                only.check(name).unwrap_err().message(),
                format!("secret '{name}' is surface-only")
            );
        }
        assert!(only.check("OPENAI_API_KEY").is_ok());
    }
}
