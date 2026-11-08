//! The profile's external memory provider (spec: memory, External memory
//! provider contract; Unavailable or disabled provider degrades loudly
//! once; Provider tools and prompt block gating; Profile-bound providers).
//!
//! `memory.provider` names `builtin` (no external provider) or an installed
//! `memory-provider` plugin; any other name fails validation. At session
//! start the named plugin is looked up among the profile's loaded plugins
//! and started for the session ([`pmt_memory::provider::ProviderSession`]).
//! A disabled, unavailable or failing provider leaves the session on
//! built-in memory with one warning per (profile, provider, reason). The
//! provider's tools join the profile's tool registry in the `memory`
//! toolset ([`tools`]) and reach the session's own provider at call time.

pub mod bridged;
pub mod setup;
pub mod tools;
pub mod wasm;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::schema::MemoryConfig;
use pmt_core::error::{Error, codes};
use pmt_core::secret::Redactor;
use pmt_memory::provider::{ProviderBackend, ProviderSession, ProviderStart, SessionInfo};
use pmt_plugins::host::CallInfo;

use crate::plugins_host::ProfilePlugins;

/// The built-in store alone.
pub const BUILTIN: &str = "builtin";
/// The world a provider plugin implements.
pub const WORLD: &str = "memory-provider";

/// Warnings already shown: (profile, provider, reason).
static WARNED: Mutex<BTreeSet<(String, String, String)>> = Mutex::new(BTreeSet::new());

/// The provider of each running session (provider tools reach it).
static ACTIVE: Mutex<BTreeMap<String, Arc<ProviderSession>>> = Mutex::new(BTreeMap::new());

/// Logs `message` at WARN once per (`profile`, `provider`, `reason`) in
/// this process (an interactive terminal shows WARN lines).
fn warn_once(profile: &str, provider: &str, reason: &str, message: &str) {
    let fresh = WARNED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((profile.to_owned(), provider.to_owned(), reason.to_owned()));
    if fresh {
        tracing::warn!(profile, provider, "{message}");
    }
}

/// Checks `memory.provider` against the plugins installed in the profile
/// (`installed`: their ids).
///
/// # Errors
///
/// `config.invalid` naming `memory.provider` and the install command.
pub fn check(config: &MemoryConfig, installed: &[String]) -> Result<(), Error> {
    let name = config.provider.as_str();
    if name == BUILTIN || installed.iter().any(|id| id == name) {
        return Ok(());
    }
    Err(Error::new(
        &codes::CONFIG_INVALID,
        format!(
            "memory.provider: unknown memory provider '{name}' (use 'builtin' or an installed memory-provider plugin)"
        ),
    )
    .with_hint(format!("prometheus plugins install {name}")))
}

/// [`check`] against `plugins`' discovered set.
///
/// # Errors
///
/// As [`check`].
pub fn check_plugins(config: &MemoryConfig, plugins: &ProfilePlugins) -> Result<(), Error> {
    let installed: Vec<String> = plugins
        .current()
        .statuses
        .iter()
        .map(|s| s.id.clone())
        .collect();
    check(config, &installed)
}

/// Why the configured provider does not run: the warning's reason key
/// and text.
type Refusal = (&'static str, String);

/// The configured provider's plugin, loaded: a `memory-provider` WASM
/// plugin, or a Hermes provider in a bridge sidecar.
///
/// # Errors
///
/// Why there is none although one is configured.
pub fn configured(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    call: &CallInfo,
) -> Result<Option<Arc<dyn ProviderBackend>>, Refusal> {
    let config = scope.config();
    let id = config.memory.provider.as_str();
    if id == BUILTIN {
        return Ok(None);
    }
    if config.plugins.disabled.iter().any(|d| d == id) {
        return Err((
            "disabled",
            format!(
                "memory provider '{id}' is disabled; using built-in memory only (enable it with: prometheus plugins enable {id})"
            ),
        ));
    }
    let loaded = plugins.current();
    if let Some(external) = loaded.memory_providers.iter().find(|m| m.plugin == id) {
        return Ok(Some(Arc::new(bridged::BridgedBackend::new(external))));
    }
    let Some(plugin) = loaded.plugins.iter().find(|p| p.manifest.id == id) else {
        let why = loaded
            .statuses
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.problem.clone())
            .unwrap_or_else(|| "it is not enabled".to_owned());
        return Err((
            "not_loaded",
            format!("memory provider '{id}' did not load ({why}); using built-in memory only"),
        ));
    };
    if !plugin.instance.implements(WORLD) {
        return Err((
            "not_a_provider",
            format!("plugin '{id}' is not a memory provider; using built-in memory only"),
        ));
    }
    Ok(Some(Arc::new(wasm::WasmBackend::new(
        Arc::clone(&plugin.instance),
        call.clone(),
    ))))
}

/// [`configured`], refusing a provider that reports itself unavailable.
fn available(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    call: &CallInfo,
) -> Result<Option<Arc<dyn ProviderBackend>>, Refusal> {
    let Some(backend) = configured(scope, plugins, call)? else {
        return Ok(None);
    };
    if backend.is_available() {
        return Ok(Some(backend));
    }
    let id = backend.id().to_owned();
    let reason = backend
        .unavailable_reason()
        .unwrap_or_else(|| "it reports itself unavailable".to_owned());
    Err((
        "unavailable",
        format!("memory provider '{id}' is unavailable: {reason}; using built-in memory only"),
    ))
}

/// Starts the profile's provider for session `session` on `platform`;
/// `None` for built-in memory only (a disabled, unavailable or failing
/// provider is warned about once).
pub async fn start(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    (session, platform): (&str, &str),
    redactor: &Redactor,
    call: &CallInfo,
) -> Option<Arc<ProviderSession>> {
    let profile = scope.name().as_str();
    let id = scope.config().memory.provider.clone();
    let backend = match available(scope, plugins, call) {
        Ok(backend) => backend?,
        Err((reason, message)) => {
            warn_once(profile, &id, reason, &message);
            return None;
        }
    };
    let started = ProviderSession::start(ProviderStart {
        backend,
        scope: scope.clone(),
        session: SessionInfo {
            session_id: session.to_owned(),
            profile: profile.to_owned(),
            platform: platform.to_owned(),
        },
        redactor: redactor.clone(),
    })
    .await;
    match started {
        Ok(provider) => {
            let provider = Arc::new(provider);
            ACTIVE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(session.to_owned(), Arc::clone(&provider));
            Some(provider)
        }
        Err(err) => {
            let message = format!(
                "memory provider '{id}' failed to start ({err}); using built-in memory only"
            );
            warn_once(profile, &id, "initialize", &message);
            None
        }
    }
}

/// The running provider of `session`.
#[must_use]
pub fn active(session: &str) -> Option<Arc<ProviderSession>> {
    ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session)
        .cloned()
}

/// Closes `session`'s provider: drains its queue, then `on-session-end`.
pub async fn close(session: &str) {
    let provider = ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(session);
    if let Some(provider) = provider {
        provider.close().await;
    }
}

/// Forgets `session`'s provider without ending it (an unloaded session).
pub fn forget(session: &str) {
    ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(session);
}
