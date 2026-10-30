//! Profile secrets: the store (`secrets.toml` or the OS keychain) and the
//! per-profile resolver (spec: configuration-and-profiles, Secrets store,
//! Secret resolution precedence and scope, Secrets are resolved per use;
//! security-boundaries, Secrets storage).
//!
//! Resolution order for a [`SecretsHandle`]: the profile's store first,
//! then, only for the process's launch profile, `PMT_SECRET_<NAME>` and the
//! provider-native variable. Store values are read at every use, so a
//! secret rotated with `prometheus secrets set` reaches the next request
//! of a running session; launch-environment values are fixed for the
//! process lifetime. Values are only ever held as [`SecretString`].

mod file;
pub mod keychain;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

pub use keychain::{Keychain, MemoryKeychain, OsKeychain};

use crate::layout::ProfileDir;
use crate::schema::{SecretsBackend, is_secret_name};
use file::FileStore;
use keychain::KeychainStore;

/// The longest a keychain value is reused from memory before it is read
/// again (the file backend is read at every use).
pub const MAX_STALENESS: Duration = Duration::from_secs(5);

/// Checks a secret name against `^[A-Z][A-Z0-9_]{0,127}$`.
///
/// # Errors
///
/// `secret.invalid_name` (exit 2).
pub fn check_name(name: &str) -> Result<(), Error> {
    if is_secret_name(name) {
        return Ok(());
    }
    Err(Error::new(
        &codes::SECRET_INVALID_NAME,
        format!("invalid secret name '{name}'"),
    )
    .with_hint("use 1-128 characters A-Z, 0-9 or '_', starting with a letter, e.g. OPENAI_API_KEY"))
}

/// Strips non-ASCII characters (pasted zero-width spaces, NBSPs) from a
/// loaded credential, with one WARN naming the key.
fn clean_value(name: &str, value: String) -> SecretString {
    if value.is_ascii() {
        return SecretString::new(value);
    }
    tracing::warn!("secret {name} contained non-ASCII characters; they were removed");
    SecretString::new(value.chars().filter(char::is_ascii).collect::<String>())
}

/// One profile's secrets store.
#[derive(Debug, Clone)]
pub struct SecretStore {
    backend: Backend,
}

#[derive(Debug, Clone)]
enum Backend {
    File(FileStore),
    Keychain(KeychainStore),
}

impl SecretStore {
    /// The store `backend` of `profile`, using the OS keychain for
    /// `keychain`.
    #[must_use]
    pub fn open(profile: &ProfileDir, backend: SecretsBackend) -> Self {
        Self::with_keychain(profile, backend, Arc::new(OsKeychain))
    }

    /// As [`SecretStore::open`] with an explicit keychain (tests use
    /// [`MemoryKeychain`]).
    #[must_use]
    pub fn with_keychain(
        profile: &ProfileDir,
        backend: SecretsBackend,
        keychain: Arc<dyn Keychain>,
    ) -> Self {
        let backend = match backend {
            SecretsBackend::File => Backend::File(FileStore::new(store_path(profile.path()))),
            SecretsBackend::Keychain => Backend::Keychain(KeychainStore::new(
                profile.home(),
                profile.name().as_str(),
                keychain,
            )),
        };
        Self { backend }
    }

    /// Which backend this is.
    #[must_use]
    pub fn kind(&self) -> SecretsBackend {
        match self.backend {
            Backend::File(_) => SecretsBackend::File,
            Backend::Keychain(_) => SecretsBackend::Keychain,
        }
    }

    /// The stored value of `name`.
    ///
    /// # Errors
    ///
    /// `secret.store` / `secret.keychain_unavailable`.
    pub fn get(&self, name: &str) -> Result<Option<SecretString>, Error> {
        match &self.backend {
            Backend::File(store) => Ok(store.load()?.remove(name)),
            Backend::Keychain(store) => store.get(name),
        }
    }

    /// Stores `value` under `name`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// `secret.invalid_name`, `secret.store`, `secret.keychain_unavailable`.
    pub fn set(&self, name: &str, value: &SecretString) -> Result<(), Error> {
        check_name(name)?;
        match &self.backend {
            Backend::File(store) => store.update(|s| {
                s.insert(name.to_owned(), value.clone());
            }),
            Backend::Keychain(store) => store.set(name, value),
        }
    }

    /// Removes `name`; `false` when it was not stored.
    ///
    /// # Errors
    ///
    /// `secret.store`, `secret.keychain_unavailable`.
    pub fn remove(&self, name: &str) -> Result<bool, Error> {
        match &self.backend {
            Backend::File(store) => store.update(|s| s.remove(name).is_some()),
            Backend::Keychain(store) => store.remove(name),
        }
    }

    /// Every stored secret name, sorted.
    ///
    /// # Errors
    ///
    /// `secret.store`, `secret.keychain_unavailable`.
    pub fn names(&self) -> Result<Vec<String>, Error> {
        match &self.backend {
            Backend::File(store) => Ok(store.load()?.into_keys().collect()),
            Backend::Keychain(store) => store.names(),
        }
    }
}

/// Moves every secret of `profile` from `from` to `to` and selects `to`
/// in its `config.toml`; returns how many moved. Everything is copied and
/// read back before the switch, and removed from `from` only after it, so
/// an interruption leaves a complete, selected store.
///
/// # Errors
///
/// Store errors; `secret.store` when a value does not read back; config
/// write errors.
pub fn migrate(profile: &ProfileDir, from: &SecretStore, to: &SecretStore) -> Result<usize, Error> {
    let names = from.names()?;
    let mut moved = Vec::new();
    for name in names {
        let Some(value) = from.get(&name)? else {
            continue;
        };
        to.set(&name, &value)?;
        if to.get(&name)?.as_ref() != Some(&value) {
            return Err(Error::new(
                &codes::SECRET_STORE,
                format!("{name} did not read back from the new backend"),
            ));
        }
        moved.push(name);
    }
    let backend = match to.kind() {
        SecretsBackend::File => "file",
        SecretsBackend::Keychain => "keychain",
    };
    profile.config_file().set("secrets.backend", backend)?;
    for name in &moved {
        from.remove(name)?;
    }
    Ok(moved.len())
}

/// `<profile>/secrets.toml`.
#[must_use]
pub fn store_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("secrets.toml")
}

/// Where a listed secret comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `<profile>/secrets.toml`.
    File,
    /// The OS keychain.
    Keychain,
    /// The launch environment (`PMT_SECRET_<NAME>` or provider-native).
    Env,
}

impl From<SecretsBackend> for Source {
    fn from(backend: SecretsBackend) -> Self {
        match backend {
            SecretsBackend::File => Self::File,
            SecretsBackend::Keychain => Self::Keychain,
        }
    }
}

/// One row of `prometheus secrets list`: never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    /// The secret name.
    pub name: String,
    /// Where it comes from.
    pub source: Source,
    /// The environment variable, for [`Source::Env`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    /// Whether a source earlier in the resolution order hides this one.
    pub shadowed: bool,
}

/// Access to one profile's secrets: its store and, for the launch profile
/// only, the secrets of the launch environment. Clones share one cache.
#[derive(Clone)]
pub struct SecretsHandle {
    profile: String,
    store: SecretStore,
    launch_env: Option<BTreeMap<String, SecretString>>,
    state: Arc<Mutex<State>>,
}

/// The keychain cache and the digest of the last value resolved per name
/// (for `/reload-secrets`, which reports changed names).
#[derive(Default)]
struct State {
    cached: BTreeMap<String, (Instant, Option<SecretString>)>,
    seen: BTreeMap<String, [u8; 32]>,
}

impl std::fmt::Debug for SecretsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsHandle")
            .field("profile", &self.profile)
            .field("backend", &self.store.kind())
            .field("launch_env", &self.launch_env.as_ref().map(BTreeMap::len))
            .finish_non_exhaustive()
    }
}

impl SecretsHandle {
    /// A handle on `store` for `profile`; `launch_env` is `Some` only for
    /// the process's launch profile.
    #[must_use]
    pub fn new(
        profile: &str,
        store: SecretStore,
        launch_env: Option<BTreeMap<String, SecretString>>,
    ) -> Self {
        Self {
            profile: profile.to_owned(),
            store,
            launch_env,
            state: Arc::default(),
        }
    }

    /// The configured backend.
    #[must_use]
    pub fn backend(&self) -> SecretsBackend {
        self.store.kind()
    }

    /// The profile's store (for `prometheus secrets`).
    #[must_use]
    pub fn store(&self) -> &SecretStore {
        &self.store
    }

    /// Whether this scope may read launch-environment secrets.
    #[must_use]
    pub fn is_launch_profile(&self) -> bool {
        self.launch_env.is_some()
    }

    /// The launch-environment value of secret `name` (`PMT_SECRET_<name>`
    /// first, then a provider-native variable `name`); always `None` for a
    /// profile other than the launch profile.
    #[must_use]
    pub fn launch_env_secret(&self, name: &str) -> Option<&SecretString> {
        self.launch_env_entry(name).map(|(_, value)| value)
    }

    fn launch_env_entry(&self, name: &str) -> Option<(String, &SecretString)> {
        let env = self.launch_env.as_ref()?;
        let generic = format!("{}{name}", pmt_core::env::SECRET_PREFIX);
        if let Some(value) = env.get(&generic) {
            return Some((generic, value));
        }
        env.get(name).map(|value| (name.to_owned(), value))
    }

    /// Resolves `name` for this use: the store (read now, or a keychain
    /// value at most [`MAX_STALENESS`] old), then the launch environment.
    ///
    /// # Errors
    ///
    /// `secret.missing` naming the secret and the command that sets it;
    /// `secret.invalid_name`; store errors.
    pub fn resolve(&self, name: &str) -> Result<SecretString, Error> {
        check_name(name)?;
        let value = match self.stored(name)? {
            Some(value) => Some(value),
            None => self.launch_env_secret(name).cloned(),
        };
        self.lock()
            .seen
            .insert(name.to_owned(), digest(value.as_ref()));
        value.ok_or_else(|| self.missing(name))
    }

    /// Drops every cached store value and returns the names, among those
    /// resolved before, whose value changed since their last use (for
    /// `/reload-secrets`; values are never returned).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn reload(&self) -> Result<Vec<String>, Error> {
        let seen = {
            let mut state = self.lock();
            state.cached.clear();
            state.seen.clone()
        };
        let mut changed = Vec::new();
        for (name, before) in seen {
            let now = match self.stored(&name)? {
                Some(value) => Some(value),
                None => self.launch_env_secret(&name).cloned(),
            };
            let now = digest(now.as_ref());
            if now != before {
                changed.push(name.clone());
            }
            self.lock().seen.insert(name, now);
        }
        Ok(changed)
    }

    /// Every secret this profile can see, store first, with the
    /// environment entries it shadows.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn list(&self) -> Result<Vec<Listed>, Error> {
        let stored = self.store.names()?;
        let mut rows: Vec<Listed> = stored
            .iter()
            .map(|name| Listed {
                name: name.clone(),
                source: self.store.kind().into(),
                variable: None,
                shadowed: false,
            })
            .collect();
        for (variable, _) in self.launch_env.iter().flatten() {
            let name = variable
                .strip_prefix(pmt_core::env::SECRET_PREFIX)
                .unwrap_or(variable);
            let winner = self.launch_env_entry(name).map(|(var, _)| var);
            rows.push(Listed {
                name: name.to_owned(),
                source: Source::Env,
                variable: Some(variable.clone()),
                shadowed: stored.iter().any(|s| s == name) || winner.as_deref() != Some(variable),
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.shadowed.cmp(&b.shadowed)));
        Ok(rows)
    }

    fn stored(&self, name: &str) -> Result<Option<SecretString>, Error> {
        if self.store.kind() == SecretsBackend::File {
            return self.store.get(name);
        }
        if let Some((at, value)) = self.lock().cached.get(name)
            && at.elapsed() < MAX_STALENESS
        {
            return Ok(value.clone());
        }
        let value = self.store.get(name)?;
        self.lock()
            .cached
            .insert(name.to_owned(), (Instant::now(), value.clone()));
        Ok(value)
    }

    fn missing(&self, name: &str) -> Error {
        let profile_flag = if self.profile == crate::layout::DEFAULT_PROFILE {
            String::new()
        } else {
            format!(" -p {}", self.profile)
        };
        Error::new(
            &codes::SECRET_MISSING,
            format!("secret '{name}' is not set for profile '{}'", self.profile),
        )
        .with_hint(format!(
            "set it with: prometheus{profile_flag} secrets set {name}"
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn digest(value: Option<&SecretString>) -> [u8; 32] {
    match value {
        Some(value) => Sha256::digest(value.expose().as_bytes()).into(),
        None => [0; 32],
    }
}

#[cfg(test)]
mod tests;
