//! The remote configuration backend (spec: configuration-and-profiles,
//! Remote configuration backend). With `config_backend.kind = "remote"`
//! (set only in `managed.toml` or `<home>/daemon.toml`) every profile's
//! configuration comes from the plane at `config_backend.url`: fetched once
//! per process (retried for up to 30 s, then a configuration error, never a
//! fallback to `config.toml` or defaults), held in memory only, refreshed
//! every `poll_secs` when a long-running process re-reads it, and written
//! back by every config writer as the changed keys only, at the profile
//! level, with optimistic concurrency.
//!
//! Networking lives in the binary: it registers a [`Transport`] at startup
//! so this crate stays free of an HTTP client.

pub mod wire;

#[cfg(test)]
pub(crate) mod fake;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::layout::ProfileName;
use crate::schema::SecretName;

/// How long the first fetch is retried before the process gives up.
pub const RETRY_WINDOW: Duration = Duration::from_secs(30);

/// The pause between two fetch attempts.
const RETRY_PAUSE: Duration = Duration::from_secs(1);

/// The default `poll_secs`.
pub const DEFAULT_POLL_SECS: u64 = 300;

/// Where profile configuration comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Each profile's `config.toml`.
    #[default]
    File,
    /// The remote configuration plane.
    Remote,
}

/// `[config_backend]` of `managed.toml` or `daemon.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigBackend {
    /// `file` (default) or `remote`.
    pub kind: BackendKind,
    /// The plane's base URL (required for `remote`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// This installation's id on the plane (required for `remote`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// The secret holding the plane's bearer token (managed secrets first,
    /// then the profile's secrets file).
    pub token_secret: SecretName,
    /// How often a long-running process re-fetches (seconds).
    #[schemars(range(min = 5, max = 86_400))]
    pub poll_secs: u64,
}

impl Default for ConfigBackend {
    fn default() -> Self {
        Self {
            kind: BackendKind::File,
            url: None,
            instance_id: None,
            token_secret: SecretName("CONFIG_PLANE_TOKEN".to_owned()),
            poll_secs: DEFAULT_POLL_SECS,
        }
    }
}

impl ConfigBackend {
    /// What is wrong with a `remote` backend's settings, if anything.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if self.kind == BackendKind::File {
            return None;
        }
        let url = self.url.as_deref().unwrap_or_default();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Some("config_backend.url must be an http(s) URL".to_owned());
        }
        if self.instance_id.as_deref().is_none_or(str::is_empty) {
            return Some("config_backend.instance_id is required".to_owned());
        }
        (!(5..=86_400).contains(&self.poll_secs))
            .then(|| "config_backend.poll_secs must be between 5 and 86400".to_owned())
    }
}

/// One HTTP answer.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status code.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

/// The binary's HTTP client for the plane.
pub trait Transport: Send + Sync {
    /// `GET url` with the bearer token.
    ///
    /// # Errors
    ///
    /// Why no answer arrived (connection refused, DNS, TLS, timeout).
    fn get(&self, url: &str, token: &SecretString) -> Result<Reply, String>;

    /// `PATCH url` with the bearer token, `If-Match: version` and a JSON
    /// body.
    ///
    /// # Errors
    ///
    /// As [`Transport::get`].
    fn patch(
        &self,
        url: &str,
        token: &SecretString,
        version: &str,
        body: &[u8],
    ) -> Result<Reply, String>;
}

static TRANSPORT: OnceLock<Arc<dyn Transport>> = OnceLock::new();

/// Registers the process's transport (the binary does at startup).
pub fn register_transport(transport: Arc<dyn Transport>) {
    let _ = TRANSPORT.set(transport);
}

/// The planes this process talks to, by home.
static PLANES: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<Plane>>>>> = OnceLock::new();

/// The plane serving `home`'s profiles, or `None` for the file backend.
///
/// # Errors
///
/// The backend settings' own errors (`managed.toml`, `daemon.toml`).
pub fn plane(home: &Path) -> Result<Option<Arc<Plane>>, Error> {
    let planes = PLANES.get_or_init(Mutex::default);
    if let Some(found) = planes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(home)
    {
        return Ok(found.clone());
    }
    let settings = settings(home)?;
    let plane = match settings.kind {
        BackendKind::File => None,
        BackendKind::Remote => {
            if let Some(problem) = settings.problem() {
                return Err(Error::new(&codes::CONFIG_INVALID, problem));
            }
            let transport = TRANSPORT.get().cloned().ok_or_else(|| {
                Error::new(
                    &codes::CONFIG_REMOTE_UNREACHABLE,
                    "this program cannot reach a remote configuration plane",
                )
            })?;
            Some(Arc::new(Plane::new(home, settings, transport)))
        }
    };
    planes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(home.to_owned(), plane.clone());
    Ok(plane)
}

/// The effective backend settings of `home`: `managed.toml`'s, else
/// `daemon.toml`'s.
///
/// # Errors
///
/// As [`plane`].
pub fn settings(home: &Path) -> Result<ConfigBackend, Error> {
    if let Some(backend) = crate::managed::current()?.and_then(crate::managed::Managed::backend) {
        return Ok(backend.clone());
    }
    Ok(*crate::daemon::DaemonConfig::load(home)?.config_backend)
}

/// One fetched profile document.
#[derive(Debug, Clone)]
struct Snapshot {
    version: String,
    layered: wire::Layered,
    at: Instant,
}

/// The remote plane of one home.
pub struct Plane {
    home: PathBuf,
    settings: ConfigBackend,
    transport: Arc<dyn Transport>,
    profiles: Mutex<BTreeMap<String, Snapshot>>,
    /// The last failed first fetch of each profile: another reader within
    /// [`RETRY_WINDOW`] of it gets the same error at once rather than
    /// waiting out a second window (one command reads a profile from
    /// several places).
    failed: Mutex<BTreeMap<String, Failure>>,
}

/// A failed first fetch.
struct Failure {
    at: Instant,
    code: &'static pmt_core::error::ErrorCode,
    message: String,
    hint: Option<String>,
}

impl Failure {
    fn of(error: &Error) -> Self {
        Self {
            at: Instant::now(),
            code: error.code(),
            message: error.message().to_owned(),
            hint: error.hint().map(str::to_owned),
        }
    }

    fn error(&self) -> Error {
        let error = Error::new(self.code, self.message.clone());
        match &self.hint {
            Some(hint) => error.with_hint(hint.clone()),
            None => error,
        }
    }
}

impl std::fmt::Debug for Plane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plane")
            .field("home", &self.home)
            .field("url", &self.url())
            .finish_non_exhaustive()
    }
}

impl Plane {
    /// A plane for `home` with `settings` over `transport` (tests build
    /// one directly).
    #[must_use]
    pub fn new(home: &Path, settings: ConfigBackend, transport: Arc<dyn Transport>) -> Self {
        Self {
            home: home.to_owned(),
            settings,
            transport,
            profiles: Mutex::default(),
            failed: Mutex::default(),
        }
    }

    /// The plane's base URL.
    #[must_use]
    pub fn url(&self) -> &str {
        self.settings.url.as_deref().unwrap_or_default()
    }

    fn document_url(&self, profile: &str) -> String {
        format!(
            "{}/v1/instances/{}/profiles/{profile}/config",
            self.url().trim_end_matches('/'),
            self.settings.instance_id.as_deref().unwrap_or_default()
        )
    }

    /// `profile`'s merged configuration text: fetched on first use (an
    /// unreachable plane fails after [`RETRY_WINDOW`]), re-fetched once it
    /// is older than `poll_secs` (a failed re-fetch keeps the snapshot).
    ///
    /// # Errors
    ///
    /// `config.remote_unreachable` (78) when the first fetch fails.
    pub fn text(&self, profile: &str) -> Result<String, Error> {
        let poll = Duration::from_secs(self.settings.poll_secs);
        if let Some(snapshot) = self.cached(profile) {
            if snapshot.at.elapsed() < poll {
                return Ok(snapshot.layered.text);
            }
            return match self.fetch_once(profile) {
                Ok(fresh) => Ok(self.store(profile, fresh)),
                Err(why) => {
                    tracing::warn!(
                        plane = self.url(),
                        profile,
                        "remote configuration refresh failed; keeping the last one: {why}"
                    );
                    self.touch(profile);
                    Ok(snapshot.layered.text)
                }
            };
        }
        // Held across the fetch: concurrent first readers wait for it.
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(snapshot) = self.cached(profile) {
            return Ok(snapshot.layered.text);
        }
        if let Some(failure) = failed.get(profile)
            && failure.at.elapsed() < RETRY_WINDOW
        {
            return Err(failure.error());
        }
        match self.fetch_retrying(profile) {
            Ok(fresh) => {
                failed.remove(profile);
                drop(failed);
                Ok(self.store(profile, fresh))
            }
            Err(error) => {
                failed.insert(profile.to_owned(), Failure::of(&error));
                Err(error)
            }
        }
    }

    /// The level locking `key` for `profile`, as last fetched.
    #[must_use]
    pub fn locked_by(&self, profile: &str, key: &str) -> Option<String> {
        let snapshot = self.cached(profile)?;
        wire::locked_by(&snapshot.layered.locks, key).map(str::to_owned)
    }

    /// Writes the keys `new` changes relative to `old` at the profile
    /// level, then re-fetches.
    ///
    /// # Errors
    ///
    /// `config.remote_locked` naming the locking level,
    /// `config.remote_conflict` (75) when the document changed since it
    /// was read, `config.remote_unreachable` when the write fails.
    ///
    /// # Panics
    ///
    /// Never: a patch of JSON values always serializes.
    pub fn write(&self, profile: &str, old: &str, new: &str) -> Result<(), Error> {
        let patch = wire::diff(old, new).map_err(|why| Error::new(&codes::CONFIG_INVALID, why))?;
        if patch.is_empty() {
            return Ok(());
        }
        for key in patch.keys() {
            if let Some(level) = self.locked_by(profile, key) {
                return Err(locked(key, &level));
            }
        }
        let version = match self.cached(profile) {
            Some(snapshot) => snapshot.version,
            None => self.fetch_retrying(profile)?.version,
        };
        let token = self.token(profile)?;
        let body = serde_json::to_vec(&patch).expect("a patch serializes");
        let reply = self
            .transport
            .patch(&self.document_url(profile), &token, &version, &body)
            .map_err(|why| self.unreachable(&why))?;
        match reply.status {
            200..=299 => {}
            409 | 412 => {
                return Err(Error::new(
                    &codes::CONFIG_REMOTE_CONFLICT,
                    format!(
                        "the configuration of profile '{profile}' changed on {} since it was read; nothing was written",
                        self.url()
                    ),
                )
                .with_hint("run the command again"));
            }
            423 => {
                let answer: serde_json::Value =
                    serde_json::from_slice(&reply.body).unwrap_or_default();
                let key = answer["key"].as_str().unwrap_or("a key");
                let level = answer["level"].as_str().unwrap_or("a higher");
                return Err(locked(key, level));
            }
            status => return Err(self.unreachable(&format!("the write answered HTTP {status}"))),
        }
        let fresh = self
            .fetch_once(profile)
            .map_err(|failed| self.unreachable(&failed.to_string()))?;
        self.store(profile, fresh);
        Ok(())
    }

    /// Stores the plane token of `source`'s secrets file in `target`'s
    /// when `target` has none and no managed token applies. The token
    /// is the installation's credential for this home's plane (one URL,
    /// one instance), kept per profile only as a fallback location; a
    /// clone without it could not load its own configuration.
    ///
    /// # Errors
    ///
    /// The secrets files' own errors.
    pub(crate) fn carry_token(
        &self,
        source: &crate::layout::ProfileDir,
        target: &crate::layout::ProfileDir,
    ) -> Result<(), Error> {
        use crate::schema::SecretsBackend;
        use crate::secrets::SecretStore;
        let name = &self.settings.token_secret.0;
        if crate::managed::current()?.is_some_and(|m| m.secret(name).is_some()) {
            return Ok(());
        }
        let to = SecretStore::open(target, SecretsBackend::File);
        if to.get(name)?.is_some() {
            return Ok(());
        }
        match SecretStore::open(source, SecretsBackend::File).get(name)? {
            Some(token) => to.set(name, &token),
            None => Ok(()),
        }
    }

    fn cached(&self, profile: &str) -> Option<Snapshot> {
        self.profiles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(profile)
            .cloned()
    }

    fn store(&self, profile: &str, snapshot: Snapshot) -> String {
        let text = snapshot.layered.text.clone();
        self.profiles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(profile.to_owned(), snapshot);
        text
    }

    /// Restarts the poll interval of a snapshot whose refresh failed.
    fn touch(&self, profile: &str) {
        if let Some(snapshot) = self
            .profiles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(profile)
        {
            snapshot.at = Instant::now();
        }
    }

    fn fetch_retrying(&self, profile: &str) -> Result<Snapshot, Error> {
        let start = Instant::now();
        loop {
            match self.fetch_once(profile) {
                Ok(snapshot) => return Ok(snapshot),
                Err(Attempt::Final(error)) => return Err(error),
                Err(Attempt::Retry(why)) if start.elapsed() + RETRY_PAUSE >= RETRY_WINDOW => {
                    return Err(self.unreachable(&why));
                }
                Err(Attempt::Retry(_)) => std::thread::sleep(RETRY_PAUSE),
            }
        }
    }

    fn fetch_once(&self, profile: &str) -> Result<Snapshot, Attempt> {
        let token = self.token(profile).map_err(Attempt::Final)?;
        let reply = self
            .transport
            .get(&self.document_url(profile), &token)
            .map_err(Attempt::Retry)?;
        match reply.status {
            200..=299 => {}
            408 | 429 | 500..=599 => {
                return Err(Attempt::Retry(format!("HTTP {}", reply.status)));
            }
            status => {
                return Err(Attempt::Final(
                    self.unreachable(&format!("the plane answered HTTP {status}")),
                ));
            }
        }
        let fetched: wire::Fetched = serde_json::from_slice(&reply.body).map_err(|e| {
            Attempt::Final(
                self.unreachable(&format!("the answer is not a configuration document: {e}")),
            )
        })?;
        let layered = wire::layer(&fetched)
            .map_err(|why| Attempt::Final(Error::new(&codes::CONFIG_INVALID, self.named(&why))))?;
        Ok(Snapshot {
            version: fetched.version,
            layered,
            at: Instant::now(),
        })
    }

    /// The bearer token: the managed secret, else the profile's secrets
    /// file.
    fn token(&self, profile: &str) -> Result<SecretString, Error> {
        let name = &self.settings.token_secret.0;
        if let Some(value) = crate::managed::current()?.and_then(|m| m.secret(name)) {
            return Ok(value.clone());
        }
        let stored = ProfileName::parse(profile).ok().and_then(|profile| {
            let dir = crate::layout::Home::open(&self.home)
                .ok()?
                .profile(&profile)
                .ok()?;
            crate::secrets::SecretStore::open(&dir, crate::schema::SecretsBackend::File)
                .get(name)
                .ok()
                .flatten()
        });
        stored.ok_or_else(|| {
            Error::new(
                &codes::CONFIG_REMOTE_UNREACHABLE,
                format!(
                    "no token for the remote configuration plane {}: secret {name} is not set",
                    self.url()
                ),
            )
            .with_hint(format!("store it with `prometheus secrets set {name}`"))
        })
    }

    fn named(&self, why: &str) -> String {
        format!("remote configuration from {}: {why}", self.url())
    }

    fn unreachable(&self, why: &str) -> Error {
        Error::new(
            &codes::CONFIG_REMOTE_UNREACHABLE,
            format!(
                "remote configuration plane {} is unreachable: {why}; config.toml is not used in its place",
                self.url()
            ),
        )
        .with_hint("check config_backend.url and the plane, or ask your administrator")
    }
}

/// One failed fetch: worth retrying or not.
enum Attempt {
    Retry(String),
    Final(Error),
}

impl std::fmt::Display for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(why) => f.write_str(why),
            Self::Final(error) => f.write_str(error.message()),
        }
    }
}

fn locked(key: &str, level: &str) -> Error {
    Error::new(
        &codes::CONFIG_REMOTE_LOCKED,
        format!("config key '{key}' is locked by the {level} level of the remote configuration"),
    )
}

#[cfg(test)]
mod tests;
