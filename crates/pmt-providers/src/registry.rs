//! The typed, profile-scoped provider registry (spec: model-providers,
//! Typed profile-scoped provider registry; Unknown and disabled providers
//! fail with typed errors).
//!
//! One registry is built per `ProfileScope` from the built-in descriptors,
//! the profile's `[providers.<id>]` blocks (overrides of a known id, or a
//! custom provider) and the descriptors of that profile's enabled provider
//! plugins. A name that is neither a canonical id nor an alias is an error,
//! never a fallback.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_config::Config;
use pmt_config::provider_urls;
use pmt_config::schema::{ProviderAuth, ProviderConfig, WireFormat};
use pmt_core::error::{Error, codes};

use crate::builtin;
use crate::descriptor::{Descriptor, HostRule, default_host_rules};

/// Where a provider came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Shipped with the binary.
    Builtin,
    /// A custom `[providers.<id>]` block.
    Config,
    /// An enabled provider plugin of the profile.
    Plugin,
}

/// A provider as this profile sees it: its descriptor and the profile's
/// `[providers.<id>]` settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    descriptor: Descriptor,
    settings: ProviderConfig,
    origin: Origin,
}

impl Provider {
    /// Canonical id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.descriptor.id
    }

    /// The descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// The profile's settings for this provider (defaults when it has no
    /// block).
    #[must_use]
    pub fn settings(&self) -> &ProviderConfig {
        &self.settings
    }

    /// Where it came from.
    #[must_use]
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// `providers.<id>.enabled`.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// The configured base URL, else the descriptor's default.
    #[must_use]
    pub fn base_url(&self) -> Option<&str> {
        self.settings
            .base_url
            .as_deref()
            .or(self.descriptor.base_url.as_deref())
    }

    /// Credential kind: the configured one, else `api_key` for a keyless
    /// provider given a `key_secret`, else the descriptor's.
    #[must_use]
    pub fn auth(&self) -> ProviderAuth {
        match self.settings.auth {
            Some(auth) => auth,
            None if self.descriptor.auth == ProviderAuth::None
                && self.settings.key_secret.is_some() =>
            {
                ProviderAuth::ApiKey
            }
            None => self.descriptor.auth,
        }
    }

    /// Whether the provider honours a prompt cache key.
    #[must_use]
    pub fn supports_prompt_cache(&self) -> bool {
        self.settings
            .supports_prompt_cache
            .unwrap_or(self.descriptor.supports_prompt_cache)
    }

    /// Whether the provider lists its models.
    #[must_use]
    pub fn supports_model_listing(&self) -> bool {
        self.settings
            .supports_model_listing
            .unwrap_or(self.descriptor.supports_model_listing)
    }

    /// The model listing URL for the effective base URL.
    #[must_use]
    pub fn models_url(&self) -> Option<String> {
        if let Some(url) = &self.settings.models_url {
            return Some(url.clone());
        }
        self.base_url()
            .map(|base| self.descriptor.models_url_for(base))
    }

    /// Hosts this provider's secrets may be sent to: the descriptor's and
    /// `providers.<id>.key_hosts`.
    #[must_use]
    pub fn key_hosts(&self) -> Vec<String> {
        let mut hosts = self.descriptor.key_hosts.clone();
        hosts.extend(self.settings.key_hosts.iter().cloned());
        hosts
    }

    /// Secret names in resolution order: `providers.<id>.key_secret`, then
    /// the descriptor's (plan-specific before shared).
    #[must_use]
    pub fn secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .settings
            .key_secret
            .iter()
            .map(|n| n.0.clone())
            .collect();
        for name in &self.descriptor.secret_names {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    /// The descriptor's host rules, then the defaults.
    #[must_use]
    pub fn host_rules(&self) -> Vec<HostRule> {
        let mut rules = self.descriptor.host_rules.clone();
        rules.extend(default_host_rules());
        rules
    }
}

/// The profile's providers by canonical id, with every alias.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Provider>,
    names: BTreeMap<String, String>,
}

impl ProviderRegistry {
    /// Builds the registry of one profile from the built-ins, `config`'s
    /// `[providers.*]` blocks and `plugins` (descriptors of the profile's
    /// enabled provider plugins only).
    ///
    /// # Errors
    ///
    /// `provider.invalid_descriptor` for a malformed descriptor, a custom
    /// block without `base_url`, a block keyed by an alias, or a name
    /// claimed twice.
    pub fn for_profile(config: &Config, plugins: Vec<Descriptor>) -> Result<Self, Error> {
        let mut registry = Self {
            providers: BTreeMap::new(),
            names: BTreeMap::new(),
        };
        for descriptor in builtin::descriptors() {
            registry.insert(descriptor, Origin::Builtin)?;
        }
        for descriptor in plugins {
            descriptor.validate()?;
            registry.insert(descriptor, Origin::Plugin)?;
        }
        for (id, settings) in &config.providers {
            registry.configure(id, settings)?;
        }
        Ok(registry)
    }

    fn insert(&mut self, descriptor: Descriptor, origin: Origin) -> Result<(), Error> {
        for name in std::iter::once(&descriptor.id).chain(&descriptor.aliases) {
            if let Some(owner) = self.names.get(name) {
                return Err(invalid(format!(
                    "provider name '{name}' of '{}' is already used by '{owner}'",
                    descriptor.id
                )));
            }
        }
        for name in std::iter::once(&descriptor.id).chain(&descriptor.aliases) {
            self.names.insert(name.clone(), descriptor.id.clone());
        }
        let id = descriptor.id.clone();
        self.providers.insert(
            id,
            Provider {
                descriptor,
                settings: ProviderConfig::default(),
                origin,
            },
        );
        Ok(())
    }

    fn configure(&mut self, id: &str, settings: &ProviderConfig) -> Result<(), Error> {
        match self.names.get(id) {
            Some(canonical) if canonical != id => Err(invalid(format!(
                "[providers.{id}]: '{id}' is an alias of '{canonical}'; configure [providers.{canonical}] instead"
            ))),
            Some(_) => {
                let provider = self.providers.get_mut(id).expect("names map to providers");
                provider.settings = settings.clone();
                for alias in &settings.aliases {
                    if self.names.contains_key(alias) {
                        return Err(invalid(format!(
                            "providers.{id}.aliases: '{alias}' is already a provider name"
                        )));
                    }
                    self.names.insert(alias.clone(), id.to_owned());
                }
                Ok(())
            }
            None => {
                let descriptor = custom_descriptor(id, settings)?;
                descriptor.validate()?;
                self.insert(descriptor, Origin::Config)?;
                self.providers.get_mut(id).expect("just inserted").settings = settings.clone();
                Ok(())
            }
        }
    }

    /// The canonical id of `name` (an id or alias), if registered.
    #[must_use]
    pub fn canonical(&self, name: &str) -> Option<&str> {
        self.names.get(name).map(String::as_str)
    }

    /// Looks a provider up by id or alias.
    ///
    /// # Errors
    ///
    /// `provider.unknown` (exit 78): `Unknown provider '<name>'.` plus the
    /// closest registered id within two edits.
    pub fn lookup(&self, name: &str) -> Result<&Provider, Error> {
        self.canonical(name)
            .and_then(|id| self.providers.get(id))
            .ok_or_else(|| self.unknown(name))
    }

    /// [`lookup`](Self::lookup), then the disabled guard.
    ///
    /// # Errors
    ///
    /// `provider.unknown`; `provider.disabled` with the message
    /// `provider '<id>' is disabled in config (providers.<id>.enabled = false)`.
    pub fn enabled(&self, name: &str) -> Result<&Provider, Error> {
        let provider = self.lookup(name)?;
        if !provider.enabled() {
            return Err(disabled(provider.id()));
        }
        Ok(provider)
    }

    fn unknown(&self, name: &str) -> Error {
        let ids = self
            .providers
            .values()
            .filter(|p| !p.descriptor.hidden)
            .map(Provider::id);
        let mut message = format!("Unknown provider '{name}'.");
        if let Some(best) = pmt_core::registry::closest(name, ids).first() {
            let _ = write!(message, " Did you mean '{best}'?");
        }
        Error::new(&codes::PROVIDER_UNKNOWN, message).with_hint("prometheus model list --providers")
    }

    /// Every provider, by canonical id.
    pub fn iter(&self) -> impl Iterator<Item = &Provider> {
        self.providers.values()
    }

    /// Providers whose descriptor names `secret` (the hosts it is bound to).
    pub fn claiming_secret<'a>(&'a self, secret: &'a str) -> impl Iterator<Item = &'a Provider> {
        self.providers
            .values()
            .filter(move |p| p.descriptor.secret_names.iter().any(|n| n == secret))
    }
}

/// The `provider.disabled` error for `id`.
#[must_use]
pub fn disabled(id: &str) -> Error {
    Error::new(
        &codes::PROVIDER_DISABLED,
        format!("provider '{id}' is disabled in config (providers.{id}.enabled = false)"),
    )
    .with_hint(format!("prometheus config set providers.{id}.enabled true"))
}

fn invalid(message: String) -> Error {
    Error::new(&codes::PROVIDER_INVALID_DESCRIPTOR, message)
}

/// The descriptor of a custom `[providers.<id>]` block.
fn custom_descriptor(id: &str, settings: &ProviderConfig) -> Result<Descriptor, Error> {
    let base_url = settings.base_url.as_deref().ok_or_else(|| {
        invalid(format!(
            "providers.{id}.base_url is required for a custom provider ('{id}' is not a built-in provider)"
        ))
    })?;
    let mut d = Descriptor::new(
        id,
        settings.wire_format.unwrap_or(WireFormat::ChatCompletions),
        Some(base_url),
    );
    d.aliases.clone_from(&settings.aliases);
    d.auth = settings.auth.unwrap_or(if settings.key_secret.is_some() {
        ProviderAuth::ApiKey
    } else {
        ProviderAuth::None
    });
    d.allow_insecure_http = settings.allow_insecure_http;
    d.local = provider_urls::host_of(base_url).is_some_and(|h| provider_urls::is_local_host(&h));
    if let Some(login) = settings.login_flow {
        d.login_flows = vec![login];
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn plugin_provider_is_invisible_to_another_profile() {
        let acme = Descriptor::new(
            "acme",
            WireFormat::ChatCompletions,
            Some("https://acme.test/v1"),
        );
        let work = ProviderRegistry::for_profile(&Config::default(), vec![acme]).unwrap();
        let home = ProviderRegistry::for_profile(&Config::default(), Vec::new()).unwrap();
        assert_eq!(work.lookup("acme").unwrap().origin(), Origin::Plugin);
        let err = home.lookup("acme").unwrap_err();
        assert_eq!(err.code().code, "provider.unknown");
        assert_eq!(err.exit_code(), 78);
    }

    #[test]
    fn alias_resolves_to_the_canonical_id() {
        let registry = ProviderRegistry::for_profile(&Config::default(), Vec::new()).unwrap();
        assert_eq!(registry.lookup("or").unwrap().id(), "openrouter");
    }

    #[test]
    fn typo_suggests_the_closest_id() {
        let registry = ProviderRegistry::for_profile(&Config::default(), Vec::new()).unwrap();
        let err = registry.lookup("opnrouter").unwrap_err();
        assert_eq!(
            err.message(),
            "Unknown provider 'opnrouter'. Did you mean 'openrouter'?"
        );
        assert_eq!(err.exit_code(), 78);
        let err = registry.lookup("zzzzzzzzzz").unwrap_err();
        assert_eq!(err.message(), "Unknown provider 'zzzzzzzzzz'.");
    }

    #[test]
    fn disabled_provider_fails_with_the_spec_message() {
        let registry = ProviderRegistry::for_profile(
            &config("[providers.openrouter]\nenabled = false\n"),
            Vec::new(),
        )
        .unwrap();
        let err = registry.enabled("or").unwrap_err();
        assert_eq!(err.code().code, "provider.disabled");
        assert_eq!(
            err.message(),
            "provider 'openrouter' is disabled in config (providers.openrouter.enabled = false)"
        );
        assert_eq!(err.exit_code(), 78);
    }

    #[test]
    fn custom_blocks_need_a_base_url_and_become_providers() {
        let err = ProviderRegistry::for_profile(
            &config("[providers.mine]\nenabled = true\n"),
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(err.code().code, "provider.invalid_descriptor");
        assert!(err.message().contains("providers.mine.base_url"));
        let registry = ProviderRegistry::for_profile(
            &config(
                "[providers.mine]\nbase_url = \"http://127.0.0.1:8080/v1\"\naliases = [\"m\"]\n",
            ),
            Vec::new(),
        )
        .unwrap();
        let mine = registry.lookup("m").unwrap();
        assert_eq!(mine.origin(), Origin::Config);
        assert_eq!(mine.auth(), ProviderAuth::None);
        assert!(mine.descriptor().local);
    }

    #[test]
    fn a_block_keyed_by_an_alias_or_a_name_claimed_twice_is_refused() {
        let err =
            ProviderRegistry::for_profile(&config("[providers.or]\nenabled = false\n"), Vec::new())
                .unwrap_err();
        assert!(err.message().contains("alias of 'openrouter'"));
        let clash = Descriptor::new("or", WireFormat::ChatCompletions, Some("https://x.test"));
        assert!(ProviderRegistry::for_profile(&Config::default(), vec![clash]).is_err());
    }
}
