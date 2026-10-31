//! The secrets, terminal and memory sections and the non-interactive
//! run.

use pmt_config::schema::ProviderAuth;
use pmt_config::{Config, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_providers::ProviderRegistry;

use super::{Wizard, plain, write_changed};
use crate::cli::SetupSection;
use crate::context::Context;

/// A secret the configured providers read, and whether it resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Required {
    /// The provider that reads it.
    pub provider: String,
    /// The secret's name (the first the provider tries).
    pub name: String,
    /// It resolves (store or environment).
    pub present: bool,
}

/// The secrets of the profile's model provider and fallback chain that
/// take an API key.
///
/// # Errors
///
/// Provider registry errors.
pub fn required_secrets(scope: &ProfileScope) -> Result<Vec<Required>, Error> {
    let config: &Config = scope.config();
    let registry = ProviderRegistry::for_profile(config, Vec::new())?;
    let mut ids: Vec<&str> = config
        .model
        .provider
        .as_deref()
        .filter(|p| *p != "auto")
        .into_iter()
        .collect();
    ids.extend(config.fallback.chain.iter().map(|e| e.provider.as_str()));
    let mut required: Vec<Required> = Vec::new();
    for id in ids {
        let Ok(provider) = registry.lookup(id) else {
            continue;
        };
        if provider.auth() != ProviderAuth::ApiKey {
            continue;
        }
        let names = provider.secret_names();
        let Some(name) = names.first() else {
            continue;
        };
        if required.iter().any(|r| r.name == *name) {
            continue;
        }
        let present = names.iter().any(|n| scope.secrets().resolve(n).is_ok());
        required.push(Required {
            provider: provider.id().to_owned(),
            name: name.clone(),
            present,
        });
    }
    Ok(required)
}

/// `setup secrets`: every required secret, asking for the missing ones.
pub(super) fn secrets(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let required = required_secrets(scope)?;
    if required.is_empty() {
        return wizard.say("Secrets — the configured providers need no API key");
    }
    wizard.say("Secrets — names only, values are never shown:")?;
    for secret in &required {
        let state = if secret.present { "set" } else { "missing" };
        wizard.say(&format!("  {} ({}): {state}", secret.name, secret.provider))?;
    }
    for secret in required.iter().filter(|s| !s.present) {
        if let Some(value) = wizard.ask_secret(&secret.name)? {
            scope.secrets().store().set(&secret.name, &value)?;
            wizard.say(&format!("{} stored in the secrets store", secret.name))?;
        }
    }
    Ok(())
}

/// `setup terminal`: the terminal backend.
pub(super) fn terminal(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let registry = pmt_sandbox::backend::BackendRegistry::builtin();
    let current = scope.config().terminal.backend.clone();
    wizard.say(&format!(
        "Terminal — backend: {current} (available: {}; Enter keeps it)",
        registry.names().join(", ")
    ))?;
    let Some(answer) = wizard.ask("terminal backend:")? else {
        return Ok(());
    };
    registry.select(&answer)?;
    if write_changed(scope, &[("terminal.backend", answer.clone())])? {
        wizard.say(&format!("terminal.backend = \"{answer}\""))?;
    }
    Ok(())
}

/// `setup memory`: the two built-in stores.
pub(super) fn memory(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    wizard.say("Memory — the agent's notes and your user profile")?;
    let mut pairs = Vec::new();
    for (key, label) in [
        ("memory.memory_enabled", "keep agent memory"),
        ("memory.user_profile_enabled", "keep a user profile"),
    ] {
        let current = scope
            .layers()
            .value(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let on = wizard.confirm(&format!("{label}?"), current)?;
        pairs.push((key, on.to_string()));
    }
    if write_changed(scope, &pairs)? {
        wizard.say("memory settings saved")?;
    }
    Ok(())
}

/// `setup gateway` before the messaging gateway exists.
pub(super) fn gateway_unavailable() -> Error {
    Error::new(
        &codes::SURFACE_NOT_INCLUDED,
        "messaging gateway setup arrives with the gateway adapters (M2); nothing to configure in this build",
    )
}

/// `setup --non-interactive`: writes the `--set` values, then checks the
/// sections' required values, taking secrets from the environment.
pub(super) fn non_interactive(ctx: &Context<'_>, sections: &[SetupSection]) -> Result<u8, Error> {
    if sections.contains(&SetupSection::Gateway) {
        return Err(gateway_unavailable());
    }
    let pairs: Vec<(&str, &str)> = ctx.set.iter().filter_map(|s| s.split_once('=')).collect();
    // The file as it is (the --set values are what gets written).
    let plain_ctx = Context { set: &[], ..*ctx };
    let scope = plain_ctx.scope()?;
    if !pairs.is_empty() {
        let owned: Vec<(&str, String)> = pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect();
        write_changed(&scope, &owned)?;
    }
    let scope = plain_ctx.scope()?;
    if sections.contains(&SetupSection::Model) && !super::model::configured(&scope) {
        return Err(missing(
            "model.provider",
            "pass --set model.provider=<id> (and model.default=<model>)",
        ));
    }
    if sections
        .iter()
        .any(|s| matches!(s, SetupSection::Model | SetupSection::Secrets))
    {
        let stored = scope.secrets().store().names()?;
        for secret in required_secrets(&scope)? {
            if stored.contains(&secret.name) {
                continue;
            }
            // From the environment into the store, so later runs need no
            // variable.
            let value = ctx.launch.secret(&secret.name).ok_or_else(|| {
                missing(
                    &secret.name,
                    &format!(
                        "export PMT_SECRET_{0} (or {0}) for this run, or run: prometheus secrets set {0}",
                        secret.name
                    ),
                )
            })?;
            scope.secrets().store().set(&secret.name, value)?;
            eprintln!("{} stored in the secrets store", secret.name);
        }
    }
    for (key, _) in &pairs {
        if let Some(value) = scope.layers().value(key) {
            eprintln!("{key} = {}", plain(value));
        }
    }
    Ok(0)
}

fn missing(what: &str, hint: &str) -> Error {
    Error::new(
        &codes::SETUP_MISSING_VALUE,
        format!("setup needs {what} and it was not given"),
    )
    .with_hint(hint.to_owned())
}
