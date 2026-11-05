//! Plugin model providers outside a session (spec: plugin-system,
//! Model-provider plugins): `prometheus model` and `prometheus auth` see
//! the providers the profile's enabled plugins register, and model
//! listings ask a plugin provider's `list-models` hook.

use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::Config;
use pmt_core::error::Error;
use pmt_plugins::host::CallInfo;
use pmt_plugins::surfaces::ModelProvider;
use pmt_providers::{PluginProvider, ProviderRegistry};

/// How stale the profile's plugin set may be for these commands (a
/// command-line process loads it fresh anyway).
const REFRESH: Duration = Duration::from_secs(10);

/// The model providers the profile's enabled plugins register.
#[must_use]
pub fn model_providers(scope: &ProfileScope) -> Vec<ModelProvider> {
    super::current(scope, REFRESH)
        .current()
        .surfaces
        .models
        .clone()
}

/// `models` as the provider registry takes them.
#[must_use]
pub fn registered(models: &[ModelProvider]) -> Vec<PluginProvider> {
    models
        .iter()
        .map(|m| PluginProvider {
            descriptor: m.descriptor.clone(),
            replaces_builtin: m.replaces_builtin,
        })
        .collect()
}

/// The registry of `config` with `scope`'s plugin providers, and those
/// providers.
///
/// # Errors
///
/// As [`ProviderRegistry::for_profile`].
pub fn registry(
    scope: &ProfileScope,
    config: &Config,
) -> Result<(ProviderRegistry, Vec<ModelProvider>), Error> {
    let models = model_providers(scope);
    let registry = ProviderRegistry::for_profile(config, registered(&models))?;
    Ok((registry, models))
}

/// The models plugin provider `id` lists through its `list-models` hook;
/// `None` when no plugin registers `id`, `Err` with the plugin's reason
/// when the hook fails.
#[must_use]
pub fn list_models(models: &[ModelProvider], id: &str) -> Option<Result<Vec<String>, String>> {
    let provider = models.iter().find(|m| m.descriptor.id == id)?;
    let call = CallInfo {
        session_id: None,
        surface: "cli".to_owned(),
        cwd: None,
    };
    Some(match provider.backend.list_models(&call, id) {
        Ok(listed) => listed,
        Err(err) => Err(err.message().to_owned()),
    })
}
