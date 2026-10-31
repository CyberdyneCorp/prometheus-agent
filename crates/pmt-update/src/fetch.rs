//! Downloads from the release origin, every file verified against the
//! embedded keys before it is parsed or used (spec: distribution-and-update,
//! Signature trust root; Update check without telemetry).
//!
//! Requests carry only `prometheus/<version> (<target>)` as the user agent:
//! no install id, profile, usage or host identifiers.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;
use serde::de::DeserializeOwned;

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
    trust: TrustRoot,
    require_transparency_log: bool,
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
            trust,
            require_transparency_log,
        })
    }

    /// The verified index of `channel`.
    ///
    /// # Errors
    ///
    /// `update.failed` (download, malformed), `update.signature` (77).
    pub async fn channel_index(&self, channel: Channel) -> Result<ChannelIndex, Error> {
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
        let file = path.rsplit('/').next().unwrap_or(path);
        let signature = self.get(&format!("{path}{SIGNATURE_SUFFIX}")).await;
        let data = self.get(path).await?;
        // A missing signature is a verification failure, not a download error.
        let signature = signature.map_err(|_| trust::failed(file))?;
        let signature = String::from_utf8(signature).map_err(|_| trust::failed(file))?;
        self.trust.verify(file, &data, &signature)?;
        if self.require_transparency_log {
            // No transparency-log verifier ships in this build: the second
            // required check cannot pass, so the file is refused.
            return Err(trust::failed(file).with_hint(
                "update.require_transparency_log = true, but this build cannot verify transparency-log signatures; set it to false to rely on the embedded keys",
            ));
        }
        Ok(data)
    }

    async fn json<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let bytes = self.verified(path).await?;
        serde_json::from_slice(&bytes).map_err(|e| malformed(path, &e.to_string()))
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/{path}", self.origin);
        let failed = |e: &dyn std::fmt::Display| {
            Error::new(&codes::UPDATE_FAILED, format!("cannot download {url}: {e}"))
        };
        let response = self.client.get(&url).send().await.map_err(|e| failed(&e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed(&status));
        }
        Ok(response.bytes().await.map_err(|e| failed(&e))?.to_vec())
    }
}

fn malformed(path: &str, why: &str) -> Error {
    Error::new(
        &codes::UPDATE_FAILED,
        format!("release file {path} is malformed: {why}"),
    )
}
