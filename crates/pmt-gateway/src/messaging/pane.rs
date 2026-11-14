//! What the setup surfaces show and write for a profile's messaging
//! adapters (spec: desktop-app, Messaging pane shows saved versus running
//! state; web-dashboard, Channel setup forms): the adapters available,
//! the secrets each one reads, each adapter's single saved-versus-running
//! state, and a credentials save that restarts nothing.
//!
//! An adapter is `pending_restart` when what it would start from now (its
//! configuration table and its secrets, fingerprinted) differs from what
//! it was last started from, or when it is enabled but was never started.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use pmt_config::ProfileScope;
use pmt_config::platforms;
use pmt_config::secrets::surface::BUILTIN;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_rpc::contracts::AdapterRow;

use super::Gateway;
use super::adapter::{AdapterState, AdapterStatus};

/// Every adapter a profile can configure: the first-party ones, then the
/// platforms enabled adapter plugins declare.
#[must_use]
pub fn available() -> Vec<String> {
    platforms::FIRST_PARTY
        .iter()
        .map(|p| p.name.to_owned())
        .chain(platforms::plugin_platforms())
        .collect()
}

/// The secrets built-in adapter `platform` reads (`MATRIX_ACCESS_TOKEN`...).
#[must_use]
pub fn secret_names(platform: &str) -> Vec<&'static str> {
    let prefix = format!("{}_", platform.to_ascii_uppercase());
    BUILTIN
        .iter()
        .copied()
        .filter(|name| name.starts_with(&prefix))
        .collect()
}

/// A fingerprint of what `platform` of `scope` would start from now.
#[must_use]
pub fn fingerprint(scope: &ProfileScope, platform: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let table = scope.config().gateway.platform(platform);
    serde_json::to_string(&table)
        .unwrap_or_default()
        .hash(&mut hasher);
    for name in secret_names(platform) {
        let value = scope.secrets().resolve(name).ok();
        value.as_ref().map(SecretString::expose).hash(&mut hasher);
    }
    hasher.finish()
}

/// The pane state of one adapter.
fn state_of(
    configured: bool,
    enabled: bool,
    changed: Option<bool>,
    status: Option<&AdapterStatus>,
) -> (&'static str, Option<String>) {
    let running = status.filter(|s| s.state != AdapterState::Disabled);
    match (running, changed) {
        (_, Some(true)) => ("pending_restart", None),
        (Some(s), _) => match s.state {
            AdapterState::Connected | AdapterState::Degraded => ("connected", None),
            AdapterState::Connecting => ("connecting", None),
            _ => (
                "error",
                Some(
                    s.message
                        .clone()
                        .unwrap_or_else(|| s.code.clone().unwrap_or_default()),
                ),
            ),
        },
        (None, _) if enabled => ("pending_restart", None),
        (None, _) if configured => ("saved", None),
        (None, _) => ("not_configured", None),
    }
}

/// Each adapter of `scope` with its state; `gateway` is the daemon's
/// messaging gateway (`None`: it does not run, every adapter is
/// `daemon_stopped`).
#[must_use]
pub fn rows(scope: &ProfileScope, gateway: Option<&Gateway>) -> Vec<AdapterRow> {
    let profile = scope.name().as_str();
    let statuses = gateway.map(Gateway::statuses).unwrap_or_default();
    let config = scope.config();
    available()
        .into_iter()
        .map(|platform| {
            let secrets: BTreeMap<String, bool> = secret_names(&platform)
                .into_iter()
                .map(|s| (s.to_owned(), scope.secrets().resolve(s).is_ok()))
                .collect();
            let table = config.gateway.platforms.get(&platform);
            let enabled = table.is_some_and(|t| t.enabled);
            let configured = table.is_some() || secrets.values().any(|set| *set);
            let (state, reason) = match gateway {
                None => ("daemon_stopped", None),
                Some(g) => {
                    let changed = super::adapters::started(&g.shared, profile, &platform)
                        .map(|was| was != fingerprint(scope, &platform));
                    let status = statuses
                        .iter()
                        .find(|s| s.profile == profile && s.platform == platform);
                    state_of(configured, enabled, changed, status)
                }
            };
            AdapterRow {
                display: platforms::first_party(&platform)
                    .map_or_else(|| platform.clone(), |p| p.display.to_owned()),
                pairing: platforms::first_party(&platform)
                    .and_then(|p| p.pairing)
                    .map(|f| f.as_str().to_owned()),
                state: state.to_owned(),
                reason,
                enabled,
                secrets,
                platform,
            }
        })
        .collect()
}

/// Why a save was refused; nothing was written.
#[derive(Debug)]
pub enum SaveError {
    /// Not a secret of this adapter.
    UnknownSecret(String),
    /// The configuration patch does not validate.
    Invalid(Error),
    /// A secret cannot be written (managed, the store failed).
    Refused(Error),
}

/// What one save writes.
#[derive(Debug, Default)]
pub struct Save<'a> {
    /// The enable switch.
    pub enabled: Option<bool>,
    /// `(option, raw text)` pairs of `gateway.platforms.<platform>.options`.
    pub options: Vec<(String, String)>,
    /// Secrets to store (non-empty values).
    pub secrets: Vec<(&'a str, &'a str)>,
    /// Secrets to delete.
    pub clear: Vec<&'a str>,
}

/// Writes `save` for `platform` of `scope`: secrets are checked first,
/// the configuration is one validated patch, and the secrets are written
/// only after it was saved. Nothing restarts.
///
/// # Errors
///
/// [`SaveError`].
pub fn save(scope: &ProfileScope, platform: &str, save: &Save<'_>) -> Result<(), SaveError> {
    let allowed = secret_names(platform);
    let named = save
        .secrets
        .iter()
        .map(|(k, _)| *k)
        .chain(save.clear.iter().copied());
    for name in named {
        if !allowed.contains(&name) {
            return Err(SaveError::UnknownSecret(name.to_owned()));
        }
        pmt_config::managed::check_secret_writable(name).map_err(SaveError::Refused)?;
    }
    let mut texts: Vec<(String, String)> = save
        .options
        .iter()
        .map(|(k, v)| {
            (
                format!("gateway.platforms.{platform}.options.{k}"),
                v.clone(),
            )
        })
        .collect();
    if let Some(enabled) = save.enabled {
        texts.push((
            format!("gateway.platforms.{platform}.enabled"),
            enabled.to_string(),
        ));
    }
    if !texts.is_empty() {
        let pairs: Vec<(&str, &str)> = texts
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        scope
            .profile()
            .config_file()
            .set_all(&pairs)
            .map_err(SaveError::Invalid)?;
    }
    let store = scope.secrets().store();
    for (key, value) in &save.secrets {
        store
            .set(key, &SecretString::new(*value))
            .map_err(SaveError::Refused)?;
    }
    for key in &save.clear {
        store.remove(key).map_err(SaveError::Refused)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AdapterState, AdapterStatus, state_of};

    fn status(state: AdapterState) -> AdapterStatus {
        AdapterStatus {
            profile: "p".into(),
            platform: "slack".into(),
            state,
            code: Some("auth".into()),
            message: Some("token rejected".into()),
            attention: false,
            paused: false,
            descriptor: None,
        }
    }

    /// Scenario: Saved but not started (the state table).
    #[test]
    fn each_adapter_has_exactly_one_state() {
        assert_eq!(state_of(true, false, None, None).0, "saved");
        assert_eq!(state_of(false, false, None, None).0, "not_configured");
        assert_eq!(state_of(true, true, None, None).0, "pending_restart");
        let connected = status(AdapterState::Connected);
        assert_eq!(
            state_of(true, true, Some(false), Some(&connected)).0,
            "connected"
        );
        assert_eq!(
            state_of(true, true, Some(true), Some(&connected)).0,
            "pending_restart"
        );
        let fatal = status(AdapterState::Fatal);
        assert_eq!(
            state_of(true, true, Some(false), Some(&fatal)),
            ("error", Some("token rejected".to_owned()))
        );
        let connecting = status(AdapterState::Connecting);
        assert_eq!(
            state_of(true, true, Some(false), Some(&connecting)).0,
            "connecting"
        );
    }
}
