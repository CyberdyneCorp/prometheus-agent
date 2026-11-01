//! Release metadata (spec: distribution-and-update, Release artifacts and
//! targets): the signed `release-manifest.json` of one release and the
//! signed channel index naming each channel's newest release.
//!
//! Layout under a release origin (GitHub's `releases/download`, a mirror,
//! or a test fixture), every file with a minisign `<file>.minisig` beside it:
//!
//! - `channels/<channel>.json`: [`ChannelIndex`]
//! - `<tag>/release-manifest.json`: [`ReleaseManifest`]
//! - `<tag>/<archive>`: one `.tar.gz` per target ([`Artifact::name`])

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::version::{Channel, ReleaseVersion};

/// `schema` of [`ReleaseManifest`].
pub const MANIFEST_SCHEMA: &str = "prometheus.release-manifest.v1";
/// `schema` of [`ChannelIndex`].
pub const INDEX_SCHEMA: &str = "prometheus.channel-index.v1";
/// File name of the manifest inside a release.
pub const MANIFEST_FILE: &str = "release-manifest.json";
/// The extension of a detached minisign signature.
pub const SIGNATURE_SUFFIX: &str = ".minisig";
/// Target name of the macOS universal (`arm64` + `x86_64`) archive.
pub const MACOS_UNIVERSAL: &str = "universal2-apple-darwin";

/// One target archive of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Target triple, or [`MACOS_UNIVERSAL`].
    pub target: String,
    /// File name, `prometheus-<target>.tar.gz`.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256 of the archive.
    pub sha256: String,
}

impl Artifact {
    /// The archive file name of `target`.
    #[must_use]
    pub fn file_name(target: &str) -> String {
        format!("prometheus-{target}.tar.gz")
    }

    /// The entry for archive `bytes` of `target`.
    #[must_use]
    pub fn of(target: &str, bytes: &[u8]) -> Self {
        Self {
            target: target.to_owned(),
            name: Self::file_name(target),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        }
    }

    /// Whether `bytes` are this archive.
    #[must_use]
    pub fn matches(&self, bytes: &[u8]) -> bool {
        bytes.len() as u64 == self.size && sha256_hex(bytes) == self.sha256
    }
}

/// `release-manifest.json`: every artifact of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// [`MANIFEST_SCHEMA`].
    pub schema: String,
    /// The release tag.
    pub version: ReleaseVersion,
    /// The commit it was built from.
    pub commit: String,
    /// RFC 3339 publication time.
    pub published_at: String,
    /// One entry per target archive.
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    /// The archive this host installs: the exact target, or on macOS the
    /// universal archive.
    #[must_use]
    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        let exact = self.artifacts.iter().find(|a| a.target == target);
        let universal = || {
            target
                .ends_with("-apple-darwin")
                .then(|| self.artifacts.iter().find(|a| a.target == MACOS_UNIVERSAL))
                .flatten()
        };
        exact.or_else(universal)
    }

    /// The manifest as published: pretty JSON with each artifact on its own
    /// line, so the POSIX installer can pick its line without a JSON parser.
    ///
    /// # Panics
    ///
    /// Never: every field serializes.
    #[must_use]
    pub fn to_published_json(&self) -> String {
        let mut out = String::from("{\n");
        let field = |v: &str| serde_json::Value::from(v).to_string();
        let _ = writeln!(out, "  \"schema\": {},", field(&self.schema));
        let _ = writeln!(out, "  \"version\": {},", field(&self.version.to_string()));
        let _ = writeln!(out, "  \"commit\": {},", field(&self.commit));
        let _ = writeln!(out, "  \"published_at\": {},", field(&self.published_at));
        out.push_str("  \"artifacts\": [\n");
        let lines: Vec<String> = self
            .artifacts
            .iter()
            .map(|a| {
                format!(
                    "    {}",
                    serde_json::to_string(a).expect("artifact serializes")
                )
            })
            .collect();
        out.push_str(&lines.join(",\n"));
        out.push_str("\n  ]\n}\n");
        out
    }
}

/// `channels/<channel>.json`: the newest release of a channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelIndex {
    /// [`INDEX_SCHEMA`].
    pub schema: String,
    /// The channel.
    pub channel: Channel,
    /// Its newest release.
    pub version: ReleaseVersion,
}

impl ChannelIndex {
    /// The index of `version` on `channel`.
    #[must_use]
    pub fn new(channel: Channel, version: ReleaseVersion) -> Self {
        Self {
            schema: INDEX_SCHEMA.to_owned(),
            channel,
            version,
        }
    }

    /// Path of a channel's index below the origin.
    #[must_use]
    pub fn path(channel: Channel) -> String {
        format!("channels/{channel}.json")
    }
}

/// Lower-case hex SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::{Artifact, MACOS_UNIVERSAL, ReleaseManifest};
    use crate::version::ReleaseVersion;

    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            schema: super::MANIFEST_SCHEMA.to_owned(),
            version: ReleaseVersion::parse("v1.4.0").unwrap(),
            commit: "abc".to_owned(),
            published_at: "2026-10-31T00:00:00Z".to_owned(),
            artifacts: vec![
                Artifact::of("x86_64-unknown-linux-musl", b"linux"),
                Artifact::of(MACOS_UNIVERSAL, b"mac"),
            ],
        }
    }

    #[test]
    fn published_json_parses_back_with_one_artifact_per_line() {
        let m = manifest();
        let text = m.to_published_json();
        let back: ReleaseManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
        let line = text
            .lines()
            .find(|l| l.contains("\"target\":\"x86_64-unknown-linux-musl\""))
            .unwrap();
        assert!(line.contains(&m.artifacts[0].sha256), "{line}");
    }

    #[test]
    fn macos_hosts_take_the_universal_archive() {
        let m = manifest();
        assert_eq!(
            m.artifact_for("aarch64-apple-darwin").unwrap().target,
            MACOS_UNIVERSAL
        );
        assert_eq!(
            m.artifact_for("x86_64-unknown-linux-musl").unwrap().name,
            "prometheus-x86_64-unknown-linux-musl.tar.gz"
        );
        assert!(m.artifact_for("x86_64-unknown-linux-gnu").is_none());
    }

    #[test]
    fn artifact_matches_only_its_bytes() {
        let a = Artifact::of("t", b"bytes");
        assert!(a.matches(b"bytes"));
        assert!(!a.matches(b"bytez"));
    }
}
