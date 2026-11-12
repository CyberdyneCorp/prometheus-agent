//! Downloads from the release origin, every file verified against the
//! embedded keys before it is parsed or used (spec: distribution-and-update,
//! Signature trust root; Update check without telemetry).
//!
//! Requests carry only `prometheus/<version> (<target>)` as the user agent:
//! no install id, profile, usage or host identifiers. Every request runs
//! under the retry budget ([`crate::download`]); with a mirror and
//! `update.mirror_fallback = true` a file the mirror fails to serve is
//! fetched from [`DEFAULT_ORIGIN`] instead (and verified the same way).

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;
use serde::de::DeserializeOwned;

use crate::download::{self, Budget};
use crate::manifest::{ChannelIndex, MANIFEST_FILE, ReleaseManifest, SIGNATURE_SUFFIX};
use crate::trust::{self, TrustRoot};
use crate::version::{Channel, ReleaseVersion};

/// Where releases are published when `update.mirror_url` is unset: GitHub
/// release assets (`channels` is a rolling release holding the indexes).
pub const DEFAULT_ORIGIN: &str =
    "https://github.com/CyberdyneCorp/prometheus-agent/releases/download";

const TIMEOUT: Duration = Duration::from_mins(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A verified download client for one origin.
#[derive(Debug, Clone)]
pub struct Releases {
    client: reqwest::Client,
    origin: String,
    fallback: Option<String>,
    budget: Budget,
    trust: TrustRoot,
    require_transparency_log: bool,
    /// An unpacked offline bundle serving the files instead of the network.
    bundle: Option<Bundled>,
}

/// The release an offline bundle carries, read from its directory.
#[derive(Debug, Clone)]
struct Bundled {
    dir: std::path::PathBuf,
    version: ReleaseVersion,
}

/// What this binary is, for the user agent.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    /// The running version.
    pub version: &'a str,
    /// The running target triple.
    pub target: &'a str,
}

impl Releases {
    /// A client for `origin` (`update.mirror_url`, else [`DEFAULT_ORIGIN`]).
    ///
    /// # Errors
    ///
    /// `net.client_init` when the HTTP client cannot be built.
    pub fn new(
        origin: Option<&str>,
        identity: Identity<'_>,
        policy: &OutboundPolicy,
        trust: TrustRoot,
        require_transparency_log: bool,
    ) -> Result<Self, Error> {
        let client = pmt_net::client_builder(policy)?
            .user_agent(format!(
                "prometheus/{} ({})",
                identity.version, identity.target
            ))
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
        Ok(Self {
            client,
            origin: origin
                .unwrap_or(DEFAULT_ORIGIN)
                .trim_end_matches('/')
                .to_owned(),
            fallback: None,
            budget: Budget::default(),
            trust,
            require_transparency_log,
            bundle: None,
        })
    }

    /// Serves the files of the offline bundle unpacked at `dir`, which
    /// carries release `version`, with no network access: every file is
    /// still verified against the embedded keys, and the channel index is
    /// the bundle's own release.
    #[must_use]
    pub fn from_bundle(mut self, dir: &std::path::Path, version: ReleaseVersion) -> Self {
        self.bundle = Some(Bundled {
            dir: dir.to_owned(),
            version,
        });
        self
    }

    /// Whether files come from an offline bundle.
    #[must_use]
    pub fn is_bundle(&self) -> bool {
        self.bundle.is_some()
    }

    /// With `update.mirror_fallback`: files a mirror fails to serve come
    /// from [`DEFAULT_ORIGIN`]. Without a mirror there is nothing to fall
    /// back from.
    #[must_use]
    pub fn with_fallback(mut self, fallback: bool) -> Self {
        let mirrored = self.origin != DEFAULT_ORIGIN;
        self.fallback = (fallback && mirrored).then(|| DEFAULT_ORIGIN.to_owned());
        self
    }

    /// Replaces the retry budget (tests use short delays).
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// The verified index of `channel`.
    ///
    /// # Errors
    ///
    /// `update.failed` (download, malformed), `update.signature` (77).
    pub async fn channel_index(&self, channel: Channel) -> Result<ChannelIndex, Error> {
        if let Some(bundle) = &self.bundle {
            return Ok(ChannelIndex::new(channel, bundle.version));
        }
        let index: ChannelIndex = self.json(&ChannelIndex::path(channel)).await?;
        if index.channel != channel {
            return Err(malformed(
                &ChannelIndex::path(channel),
                "names another channel",
            ));
        }
        Ok(index)
    }

    /// The verified manifest of release `version`.
    ///
    /// # Errors
    ///
    /// `update.failed` (download, malformed), `update.signature` (77).
    pub async fn manifest(&self, version: &ReleaseVersion) -> Result<ReleaseManifest, Error> {
        let path = format!("{version}/{MANIFEST_FILE}");
        let manifest: ReleaseManifest = self.json(&path).await?;
        if manifest.version != *version {
            return Err(malformed(&path, "describes another release"));
        }
        Ok(manifest)
    }

    /// Downloads and verifies the signed file at `path`.
    ///
    /// # Errors
    ///
    /// `update.failed` (download), `update.signature` (77).
    pub async fn verified(&self, path: &str) -> Result<Vec<u8>, Error> {
        self.verified_with_signature(path)
            .await
            .map(|(data, _)| data)
    }

    /// As [`Releases::verified`], reporting `downloading <n>%` lines (every
    /// 10 %) of a file of `total` bytes to `progress`.
    ///
    /// # Errors
    ///
    /// As [`Releases::verified`].
    pub async fn verified_with_progress(
        &self,
        path: &str,
        total: u64,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<Vec<u8>, Error> {
        if self.bundle.is_some() {
            return self.verified(path).await;
        }
        let file = path.rsplit('/').next().unwrap_or(path);
        let signature = self.get(&format!("{path}{SIGNATURE_SUFFIX}")).await;
        let url = format!("{}/{path}", self.origin);
        let mut sink = Progressed {
            bytes: Vec::new(),
            total,
            reported: 0,
            progress,
        };
        let fetched = download::fetch(&self.client, &url, &mut sink, &self.budget).await;
        let data = match (fetched, &self.fallback) {
            (Ok(_), _) => sink.bytes,
            // The mirror failed: the fallback origin, without progress.
            (Err(_), Some(_)) => self.get(path).await?,
            (Err(e), None) => {
                return Err(Error::new(
                    &codes::UPDATE_FAILED,
                    format!("cannot download {url}: {e}"),
                ));
            }
        };
        let signature = signature.map_err(|_| trust::failed(file))?;
        let signature = String::from_utf8(signature).map_err(|_| trust::failed(file))?;
        self.check(file, &data, &signature)?;
        Ok(data)
    }

    fn check(&self, file: &str, data: &[u8], signature: &str) -> Result<(), Error> {
        self.trust.verify(file, data, signature)?;
        if self.require_transparency_log {
            // No transparency-log verifier ships in this build: the second
            // required check cannot pass, so the file is refused.
            return Err(trust::failed(file).with_hint(
                "update.require_transparency_log = true, but this build cannot verify transparency-log signatures; set it to false to rely on the embedded keys",
            ));
        }
        Ok(())
    }

    /// As [`Releases::verified`], also answering the signature text (to
    /// keep beside a file that is verified again at each load).
    ///
    /// # Errors
    ///
    /// As [`Releases::verified`].
    pub async fn verified_with_signature(&self, path: &str) -> Result<(Vec<u8>, String), Error> {
        let file = path.rsplit('/').next().unwrap_or(path);
        let signature = self.get(&format!("{path}{SIGNATURE_SUFFIX}")).await;
        let data = self.get(path).await?;
        // A missing signature is a verification failure, not a download error.
        let signature = signature.map_err(|_| trust::failed(file))?;
        let signature = String::from_utf8(signature).map_err(|_| trust::failed(file))?;
        self.check(file, &data, &signature)?;
        Ok((data, signature))
    }

    /// Downloads `url` under the retry budget (not verified: the caller
    /// checks a pinned digest, as for runtime archives).
    ///
    /// # Errors
    ///
    /// `update.failed` when it cannot be downloaded.
    pub async fn fetch_url(&self, url: &str) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        download::fetch(&self.client, url, &mut bytes, &self.budget)
            .await
            .map_err(|e| {
                Error::new(&codes::UPDATE_FAILED, format!("cannot download {url}: {e}"))
            })?;
        Ok(bytes)
    }

    async fn json<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let bytes = self.verified(path).await?;
        serde_json::from_slice(&bytes).map_err(|e| malformed(path, &e.to_string()))
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, Error> {
        if let Some(bundle) = &self.bundle {
            let file = bundle.dir.join(path);
            return std::fs::read(&file).map_err(|e| {
                Error::new(
                    &codes::UPDATE_FAILED,
                    format!("the bundle has no {path}: {e}"),
                )
            });
        }
        let primary = self.get_from(&self.origin, path).await;
        match (&self.fallback, primary) {
            (Some(origin), Err(err)) => {
                tracing::warn!(component = "update", "{}; trying {origin}", err.message());
                self.get_from(origin, path).await
            }
            (_, result) => result,
        }
    }

    async fn get_from(&self, origin: &str, path: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{origin}/{path}");
        let mut bytes = Vec::new();
        download::fetch(&self.client, &url, &mut bytes, &self.budget)
            .await
            .map_err(|e| {
                Error::new(&codes::UPDATE_FAILED, format!("cannot download {url}: {e}"))
            })?;
        Ok(bytes)
    }
}

/// A download into memory reporting `downloading <n>%` every 10 %.
struct Progressed<'a> {
    bytes: Vec<u8>,
    total: u64,
    reported: u64,
    progress: &'a (dyn Fn(&str) + Sync),
}

impl download::Sink for Progressed<'_> {
    fn written(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        let percent = (self.bytes.len() as u64 * 100)
            .checked_div(self.total)
            .unwrap_or(100)
            .min(100);
        if percent >= self.reported + 10 || (percent == 100 && self.reported < 100) {
            self.reported = percent - percent % 10;
            (self.progress)(&format!("downloading {}%", self.reported));
        }
        Ok(())
    }

    fn reset(&mut self) -> std::io::Result<()> {
        self.bytes.clear();
        self.reported = 0;
        Ok(())
    }
}

fn malformed(path: &str, why: &str) -> Error {
    Error::new(
        &codes::UPDATE_FAILED,
        format!("release file {path} is malformed: {why}"),
    )
}
