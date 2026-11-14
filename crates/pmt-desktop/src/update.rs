//! Signed in-app updates (spec: desktop-app, Signed in-app updates;
//! Development builds have no updater).
//!
//! A release build embeds the release signing key (the release workflow
//! sets `PMT_DESKTOP_RELEASE_KEY` when it builds the app) and the channel
//! it was built for. It reads the channel's feed
//! `<origin>/channels/desktop-<channel>.json` (signed like every release
//! file, `.minisig` next to it), offers a newer version, downloads the
//! platform's update archive only when the user accepts (apply), verifies
//! its signature against the embedded key before anything is written, and
//! stages it; the app installs the staged archive when it quits or
//! restarts ([`crate::update_install`]). The daemon is never stopped here:
//! the next launch finds it stale against the new bundled binary and
//! restarts it then. A build without the key has no updater at all and
//! makes no network request.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

/// What check and apply answer in a build without an embedded key.
pub const NO_UPDATES: &str =
    "This build doesn't get updates. Install a release build to receive updates.";
/// A second apply while one runs.
pub const IN_PROGRESS: &str = "An update is already in progress.";
/// A download whose signature does not verify.
pub const SIGNATURE_INVALID: &str = "Update signature invalid; not installed";
/// Background checks run at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_hours(6);
/// Where releases are published when `update.mirror_url` is unset (the
/// same origin as `pmt_update::DEFAULT_ORIGIN`, which the CLI's updates
/// use; a test of the `prometheus` crate keeps them equal).
pub const DEFAULT_ORIGIN: &str =
    "https://github.com/CyberdyneCorp/prometheus-agent/releases/download";
/// The feed's schema id.
pub const FEED_SCHEMA: &str = "prometheus.desktop-feed.v1";

const LAST_CHECK_FILE: &str = "last-check.json";
const STAGED_DIR: &str = "staged";
const STAGED_FILE: &str = "staged.json";

/// A future the [`Fetch`] seam returns.
pub type Fetching<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// Downloads a URL's bytes (the app's HTTP client, a fake in tests).
pub trait Fetch: Send + Sync {
    /// The body of `url`.
    fn get<'a>(&'a self, url: &'a str) -> Fetching<'a>;
}

/// How this build is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    /// No embedded key: no updater (local and commit builds).
    None,
    /// Signed archives from the release channel.
    Signed,
}

/// What the app was built as and where its updates come from.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// The embedded minisign public key (the `.pub` file text); `None` in
    /// development builds.
    pub release_key: Option<String>,
    /// `stable` or `canary`.
    pub channel: String,
    /// The release origin (`update.mirror_url`, else the GitHub releases).
    pub origin: String,
    /// The feed's platform key (`darwin-universal`, `linux-x86_64`...).
    pub platform: String,
    /// The running version.
    pub current: String,
    /// Where the last check time and the staged archive live.
    pub dir: PathBuf,
}

/// The channel feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    /// [`FEED_SCHEMA`].
    pub schema: String,
    /// The channel's current version.
    pub version: String,
    /// One line of release notes.
    #[serde(default)]
    pub notes: String,
    /// Update archives by platform key, published under the release's
    /// tag (`<origin>/v<version>/<file>`).
    pub platforms: std::collections::BTreeMap<String, Archive>,
}

/// One platform's update archive in the feed. Its digest binds the
/// archive to this signed feed: an archive's own signature only says the
/// release key signed it once, so an older signed build served at the new
/// version's URL would otherwise install as that version (a downgrade).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    /// Its file name.
    pub file: String,
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
}

impl Archive {
    /// The entry of `file` with `bytes`.
    #[must_use]
    pub fn of(file: &str, bytes: &[u8]) -> Self {
        Self {
            file: file.to_owned(),
            sha256: sha256_hex(bytes),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A newer version on the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// Its version.
    pub version: String,
    /// Its notes.
    pub notes: String,
    /// The archive's URL.
    pub url: String,
    /// The archive's SHA-256 the signed feed names.
    pub sha256: String,
}

/// A verified archive waiting for the next quit or restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    /// Its version.
    pub version: String,
    /// The archive.
    pub archive: PathBuf,
}

/// The answer to check and apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// How this build is updated.
    pub mechanism: Mechanism,
    /// Whether the step did what was asked.
    pub ok: bool,
    /// A newer version, after a check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<Offer>,
    /// The version staged for the next restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<String>,
    /// What to tell the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Outcome {
    fn none() -> Self {
        Self {
            mechanism: Mechanism::None,
            ok: false,
            available: None,
            staged: None,
            message: Some(NO_UPDATES.to_owned()),
        }
    }

    fn signed(ok: bool, message: Option<String>) -> Self {
        Self {
            mechanism: Mechanism::Signed,
            ok,
            available: None,
            staged: None,
            message,
        }
    }
}

/// The app's updater.
pub struct Updater {
    config: Option<(UpdaterConfig, PublicKey)>,
    fetch: Option<Box<dyn Fetch>>,
    offer: Mutex<Option<Offer>>,
    busy: AtomicBool,
}

impl Updater {
    /// The updater of a build without an embedded key.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            config: None,
            fetch: None,
            offer: Mutex::new(None),
            busy: AtomicBool::new(false),
        }
    }

    /// The updater of `config`; disabled when it has no key (or a key
    /// that does not parse, which a release build never has).
    #[must_use]
    pub fn new(config: UpdaterConfig, fetch: Box<dyn Fetch>) -> Self {
        let key = config
            .release_key
            .as_deref()
            .and_then(|text| PublicKey::decode(text.trim()).ok());
        let Some(key) = key else {
            return Self::disabled();
        };
        Self {
            config: Some((config, key)),
            fetch: Some(fetch),
            offer: Mutex::new(None),
            busy: AtomicBool::new(false),
        }
    }

    /// How this build is updated.
    #[must_use]
    pub fn mechanism(&self) -> Mechanism {
        if self.config.is_some() {
            Mechanism::Signed
        } else {
            Mechanism::None
        }
    }

    /// Looks for a newer version. A background check (`explicit` false)
    /// within [`CHECK_INTERVAL`] of the last one answers from memory.
    pub async fn check(&self, explicit: bool) -> Outcome {
        let (Some((config, key)), Some(fetch)) = (&self.config, &self.fetch) else {
            return Outcome::none();
        };
        if !explicit && checked_recently(&config.dir, SystemTime::now()) {
            return self.offered(config);
        }
        let found = latest(config, key, fetch.as_ref()).await;
        record_check(&config.dir, SystemTime::now());
        match found {
            Ok(offer) => {
                *self
                    .offer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = offer;
                self.offered(config)
            }
            Err(message) => Outcome::signed(false, Some(message)),
        }
    }

    fn offered(&self, config: &UpdaterConfig) -> Outcome {
        let offer = self
            .offer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let message = offer
            .is_none()
            .then(|| "Prometheus is up to date.".to_owned());
        Outcome {
            available: offer,
            staged: staged(&config.dir).map(|s| s.version),
            ..Outcome::signed(true, message)
        }
    }

    /// Downloads, verifies and stages the offered version (the user
    /// accepted it). Nothing is written unless the signature verifies.
    pub async fn apply(&self) -> Outcome {
        let (Some((config, key)), Some(fetch)) = (&self.config, &self.fetch) else {
            return Outcome::none();
        };
        if self.busy.swap(true, Ordering::SeqCst) {
            return Outcome::signed(false, Some(IN_PROGRESS.to_owned()));
        }
        let outcome = self.download(config, key, fetch.as_ref()).await;
        self.busy.store(false, Ordering::SeqCst);
        outcome
    }

    async fn download(
        &self,
        config: &UpdaterConfig,
        key: &PublicKey,
        fetch: &dyn Fetch,
    ) -> Outcome {
        let offer = self
            .offer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(offer) = offer else {
            return Outcome::signed(
                false,
                Some("No update to install; check for updates first.".to_owned()),
            );
        };
        let archive = match verified(fetch, key, &offer.url).await {
            Ok(bytes) if sha256_hex(&bytes) == offer.sha256.to_ascii_lowercase() => bytes,
            Ok(_) => {
                tracing::warn!(
                    version = offer.version,
                    "discarded an update archive the signed feed does not name"
                );
                return Outcome::signed(false, Some(SIGNATURE_INVALID.to_owned()));
            }
            Err(Failure::Signature) => {
                tracing::warn!(
                    version = offer.version,
                    "discarded an update whose signature does not verify"
                );
                return Outcome::signed(false, Some(SIGNATURE_INVALID.to_owned()));
            }
            Err(Failure::Fetch(e)) => return Outcome::signed(false, Some(e)),
        };
        match stage(&config.dir, &offer, &archive) {
            Ok(staged) => Outcome {
                staged: Some(staged.version.clone()),
                ..Outcome::signed(
                    true,
                    Some(format!(
                        "Prometheus {} will be installed when you quit or restart the app.",
                        staged.version
                    )),
                )
            },
            Err(e) => Outcome::signed(false, Some(format!("could not stage the update: {e}"))),
        }
    }

    /// The archive staged for the next quit or restart.
    #[must_use]
    pub fn staged(&self) -> Option<Staged> {
        self.config.as_ref().and_then(|(c, _)| staged(&c.dir))
    }
}

enum Failure {
    Signature,
    Fetch(String),
}

/// `url`'s bytes, verified against `key` by `<url>.minisig`.
async fn verified(fetch: &dyn Fetch, key: &PublicKey, url: &str) -> Result<Vec<u8>, Failure> {
    let bytes = fetch.get(url).await.map_err(Failure::Fetch)?;
    let signature = fetch
        .get(&format!("{url}.minisig"))
        .await
        .map_err(Failure::Fetch)?;
    let signature = std::str::from_utf8(&signature)
        .ok()
        .and_then(|text| Signature::decode(text).ok())
        .ok_or(Failure::Signature)?;
    key.verify(&bytes, &signature, false)
        .map_err(|_| Failure::Signature)?;
    Ok(bytes)
}

/// The channel's newer version for this platform, if any.
async fn latest(
    config: &UpdaterConfig,
    key: &PublicKey,
    fetch: &dyn Fetch,
) -> Result<Option<Offer>, String> {
    let origin = config.origin.trim_end_matches('/');
    let url = format!("{origin}/channels/desktop-{}.json", config.channel);
    let bytes = verified(fetch, key, &url).await.map_err(|e| match e {
        Failure::Signature => SIGNATURE_INVALID.to_owned(),
        Failure::Fetch(e) => format!("could not check for updates: {e}"),
    })?;
    let feed: Feed =
        serde_json::from_slice(&bytes).map_err(|e| format!("the update feed is malformed: {e}"))?;
    if feed.schema != FEED_SCHEMA {
        return Err(format!("the update feed has schema {}", feed.schema));
    }
    if !newer(&feed.version, &config.current) {
        return Ok(None);
    }
    let Some(archive) = feed.platforms.get(&config.platform) else {
        return Ok(None);
    };
    Ok(Some(Offer {
        url: format!("{origin}/v{}/{}", feed.version, archive.file),
        sha256: archive.sha256.clone(),
        version: feed.version,
        notes: feed.notes,
    }))
}

fn newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(candidate), Ok(current)) => candidate > current,
        _ => false,
    }
}

#[derive(Serialize, Deserialize)]
struct LastCheck {
    at_unix: u64,
}

fn unix(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn checked_recently(dir: &Path, now: SystemTime) -> bool {
    std::fs::read(dir.join(LAST_CHECK_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LastCheck>(&bytes).ok())
        .is_some_and(|last| unix(now).saturating_sub(last.at_unix) < CHECK_INTERVAL.as_secs())
}

fn record_check(dir: &Path, now: SystemTime) {
    let text = serde_json::json!({"at_unix": unix(now)}).to_string();
    if let Err(e) =
        std::fs::create_dir_all(dir).and_then(|()| std::fs::write(dir.join(LAST_CHECK_FILE), text))
    {
        tracing::warn!("could not record the update check: {e}");
    }
}

fn stage(dir: &Path, offer: &Offer, archive: &[u8]) -> std::io::Result<Staged> {
    let staged_dir = dir.join(STAGED_DIR);
    // One staged update at a time: a newer one replaces an older one.
    let _ = std::fs::remove_dir_all(&staged_dir);
    std::fs::create_dir_all(&staged_dir)?;
    let name = offer.url.rsplit('/').next().unwrap_or("update");
    let path = staged_dir.join(name);
    std::fs::write(&path, archive)?;
    let staged = Staged {
        version: offer.version.clone(),
        archive: path,
    };
    std::fs::write(
        staged_dir.join(STAGED_FILE),
        serde_json::to_vec(&staged).map_err(std::io::Error::other)?,
    )?;
    Ok(staged)
}

/// The archive staged under `dir`, if any.
#[must_use]
pub fn staged_in(dir: &Path) -> Option<Staged> {
    staged(dir)
}

fn staged(dir: &Path) -> Option<Staged> {
    let bytes = std::fs::read(dir.join(STAGED_DIR).join(STAGED_FILE)).ok()?;
    serde_json::from_slice::<Staged>(&bytes)
        .ok()
        .filter(|s| s.archive.is_file())
}

/// Forgets the staged archive (installed, or abandoned).
pub fn clear_staged(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.join(STAGED_DIR));
}

/// The app's [`Fetch`]: the shared outbound HTTP client.
pub struct HttpFetch {
    client: reqwest::Client,
}

impl HttpFetch {
    /// A client with the default outbound policy.
    ///
    /// # Errors
    ///
    /// When the HTTP client cannot be built.
    pub fn new(user_agent: &str) -> Result<Self, String> {
        let client = pmt_net::client_builder(&pmt_net::http::OutboundPolicy::default())
            .map_err(|e| e.to_string())?
            .user_agent(user_agent)
            .timeout(Duration::from_mins(10))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

impl Fetch for HttpFetch {
    fn get<'a>(&'a self, url: &'a str) -> Fetching<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| e.to_string())?;
            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{checked_recently, newer, record_check};

    #[test]
    fn only_a_higher_semantic_version_is_newer() {
        assert!(newer("1.6.0", "1.5.9"));
        assert!(newer("1.6.0", "1.6.0-canary.20261101"));
        assert!(!newer("1.6.0", "1.6.0"));
        assert!(!newer("1.5.0", "1.6.0"));
        assert!(!newer("garbage", "1.6.0"));
    }

    #[test]
    fn a_check_counts_for_six_hours() {
        let dir = tempfile::tempdir().unwrap();
        let then = SystemTime::now() - Duration::from_hours(5);
        assert!(!checked_recently(dir.path(), SystemTime::now()));
        record_check(dir.path(), then);
        assert!(checked_recently(dir.path(), SystemTime::now()));
        assert!(!checked_recently(
            dir.path(),
            SystemTime::now() + Duration::from_hours(2)
        ));
    }
}
