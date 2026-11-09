//! The runtime index pinned by this binary (spec: distribution-and-update,
//! Managed runtime store; Reproducible, locked builds): per runtime one
//! version and, per host platform, the archive URL, size and SHA-256.
//!
//! The index is `runtimes/index.json` in this crate, compiled in, so it is
//! signed together with the binary; `scripts/pin-runtimes` regenerates it.
//! Its digest is printed by `prometheus --version --verbose`.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::manifest::sha256_hex;

/// The embedded index file.
const EMBEDDED: &str = include_str!("../../runtimes/index.json");
/// `schema` of the index.
pub const INDEX_SCHEMA: &str = "prometheus.runtime-index.v1";

/// How an archive is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// gzip-compressed tar.
    #[serde(rename = "tar.gz")]
    TarGz,
    /// zip (deflate).
    #[serde(rename = "zip")]
    Zip,
}

/// One pinned archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedArtifact {
    /// Where the default origin serves it.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Packing.
    pub format: Format,
    /// The runtime's executable, relative to the unpacked archive.
    pub exe: String,
}

impl PinnedArtifact {
    /// The archive's file name (the URL's last segment).
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

/// One runtime's pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The pinned version.
    pub version: String,
    /// Archives by host platform ([`super::platform`]).
    pub artifacts: BTreeMap<String, PinnedArtifact>,
}

impl Pin {
    /// The store directory name of `artifact`: `<version>-<sha12>`.
    #[must_use]
    pub fn dir_name(&self, artifact: &PinnedArtifact) -> String {
        format!("{}-{}", self.version, &artifact.sha256[..12])
    }
}

/// The whole index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIndex {
    /// [`INDEX_SCHEMA`].
    pub schema: String,
    /// Pins by runtime name.
    pub runtimes: BTreeMap<String, Pin>,
}

impl RuntimeIndex {
    /// The index compiled into this binary.
    ///
    /// # Panics
    ///
    /// Never for the committed file (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("the embedded runtime index parses")
    }

    /// Lower-case hex SHA-256 of the embedded index file.
    #[must_use]
    pub fn embedded_digest() -> String {
        sha256_hex(EMBEDDED.as_bytes())
    }

    /// The runtime names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.runtimes.keys().map(String::as_str).collect()
    }

    /// The pin of `name`.
    ///
    /// # Errors
    ///
    /// `runtime.unknown` (exit 2) `unknown runtime '<n>' (expected
    /// node|cpython|uv|chromium|doc-converter)`.
    pub fn pin(&self, name: &str) -> Result<&Pin, Error> {
        self.runtimes.get(name).ok_or_else(|| {
            Error::new(
                &codes::RUNTIME_UNKNOWN,
                format!(
                    "unknown runtime '{name}' (expected {})",
                    self.expected_names()
                ),
            )
        })
    }

    /// The pin of `name` and its archive for `platform`.
    ///
    /// # Errors
    ///
    /// `runtime.unknown` (2); `runtime.unsupported` (69) when the runtime
    /// has no archive for `platform`.
    pub fn artifact(&self, name: &str, platform: &str) -> Result<(&Pin, &PinnedArtifact), Error> {
        let pin = self.pin(name)?;
        let artifact = pin.artifacts.get(platform).ok_or_else(|| {
            Error::new(
                &codes::RUNTIME_UNSUPPORTED,
                format!(
                    "{name} {} has no pinned archive for {platform}",
                    pin.version
                ),
            )
        })?;
        Ok((pin, artifact))
    }

    /// The store directory names this index pins on `platform`, by
    /// runtime name (`version --json` reports them for `runtimes gc`).
    #[must_use]
    pub fn pins_for(&self, platform: &str) -> BTreeMap<String, String> {
        self.runtimes
            .iter()
            .filter_map(|(name, pin)| {
                let artifact = pin.artifacts.get(platform)?;
                Some((name.clone(), pin.dir_name(artifact)))
            })
            .collect()
    }

    /// The pinned names in the spec's order (`node|cpython|uv|chromium|doc-converter`):
    /// the order of `pmt_config::daemon::RUNTIME_NAMES`.
    fn expected_names(&self) -> String {
        pmt_config::daemon::RUNTIME_NAMES
            .iter()
            .filter(|n| self.runtimes.contains_key(**n))
            .copied()
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::{INDEX_SCHEMA, RuntimeIndex};

    #[test]
    fn embedded_index_pins_exactly_the_configurable_names() {
        let index = RuntimeIndex::embedded();
        assert_eq!(index.schema, INDEX_SCHEMA);
        let mut names = index.names();
        let mut expected = pmt_config::daemon::RUNTIME_NAMES.to_vec();
        names.sort_unstable();
        expected.sort_unstable();
        assert_eq!(names, expected);
        for (name, pin) in &index.runtimes {
            assert!(!pin.artifacts.is_empty(), "{name} has no archives");
            for (platform, a) in &pin.artifacts {
                assert_eq!(a.sha256.len(), 64, "{name} {platform}");
                assert!(a.url.starts_with("https://"), "{name} {platform}");
                assert!(a.size > 0, "{name} {platform}");
            }
        }
    }

    #[test]
    fn unknown_runtime_names_the_expected_ones() {
        let err = RuntimeIndex::embedded().pin("java").unwrap_err();
        assert_eq!(
            err.message(),
            "unknown runtime 'java' (expected node|cpython|uv|chromium|doc-converter)"
        );
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn every_platform_of_this_host_family_pins_node() {
        let index = RuntimeIndex::embedded();
        for platform in [
            "darwin-arm64",
            "darwin-x64",
            "linux-x64",
            "linux-arm64",
            "linux-x64-musl",
            "linux-arm64-musl",
            "windows-x64",
            "windows-arm64",
        ] {
            assert!(index.artifact("node", platform).is_ok(), "{platform}");
            assert!(index.artifact("uv", platform).is_ok(), "{platform}");
            assert!(index.artifact("cpython", platform).is_ok(), "{platform}");
        }
        assert!(index.artifact("chromium", "linux-x64-musl").is_err());
    }
}
