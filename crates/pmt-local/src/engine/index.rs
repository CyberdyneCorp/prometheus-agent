//! The llama.cpp builds this binary pins (spec: local-models, Hash-pinned
//! engine installation): one engine version, and per `<platform>/<backend>`
//! the archives (the engine and, for CUDA, its runtime libraries) with
//! their URL, size and SHA-256. The index is `engine.json` in this crate,
//! compiled in and signed with the binary; there is no config key to
//! override the version.

use std::collections::BTreeMap;

use pmt_config::daemon_local::LocalBackend;
use pmt_core::error::{Error, codes};
use pmt_update::runtimes::index::Format;
use serde::{Deserialize, Serialize};

const EMBEDDED: &str = include_str!("../../engine.json");
/// `schema` of the index.
pub const SCHEMA: &str = "prometheus.llamacpp-engine.v1";

/// One pinned archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineFile {
    /// Where it is downloaded from.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Packing.
    pub format: Format,
}

impl EngineFile {
    /// The archive's file name (the URL's last segment).
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

/// One build: its archives, unpacked into one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The archives.
    pub files: Vec<EngineFile>,
    /// `llama-server`, relative to the install directory.
    pub exe: String,
    /// Directories (relative) the dynamic loader must search: the engine's
    /// and the CUDA runtime's shared libraries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library_dirs: Vec<String>,
}

/// The whole index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineIndex {
    /// [`SCHEMA`].
    pub schema: String,
    /// The pinned llama.cpp release.
    pub version: String,
    /// Builds by `<platform>/<backend>`.
    pub builds: BTreeMap<String, Build>,
}

impl EngineIndex {
    /// The index compiled into this binary.
    ///
    /// # Panics
    ///
    /// Never for the committed file (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("the embedded engine index parses")
    }

    /// The build of `backend` for `platform`.
    ///
    /// # Errors
    ///
    /// `runtime.unsupported` (exit 69) naming the pair when this release
    /// has no such build: refused before any download.
    pub fn build(&self, platform: &str, backend: LocalBackend) -> Result<&Build, Error> {
        self.builds
            .get(&format!("{platform}/{backend}"))
            .ok_or_else(|| {
                let supported: Vec<&str> = self
                    .builds
                    .keys()
                    .filter_map(|k| k.strip_prefix(&format!("{platform}/")))
                    .collect();
                Error::new(
                    &codes::RUNTIME_UNSUPPORTED,
                    format!(
                        "llama.cpp {} has no build for ({backend}, {platform})",
                        self.version
                    ),
                )
                .with_hint(if supported.is_empty() {
                    format!("no local model backend runs on {platform}")
                } else {
                    format!(
                        "set local_runtime.backend in daemon.toml to one of: auto, {}",
                        supported.join(", ")
                    )
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_local::LocalBackend;

    use super::{EngineIndex, SCHEMA};

    #[test]
    fn the_embedded_index_pins_every_supported_pair() {
        let index = EngineIndex::embedded();
        assert_eq!(index.schema, SCHEMA);
        for (key, build) in &index.builds {
            assert!(!build.files.is_empty(), "{key}");
            for file in &build.files {
                assert_eq!(file.sha256.len(), 64, "{key}");
                assert!(file.url.starts_with("https://"), "{key}");
                assert!(file.size > 0, "{key}");
                assert!(file.url.contains(&index.version), "{key} pins one release");
            }
        }
        for pair in [
            ("darwin-arm64", LocalBackend::Metal),
            ("linux-x64", LocalBackend::Cuda),
            ("linux-x64", LocalBackend::Vulkan),
            ("linux-x64", LocalBackend::Hip),
            ("linux-arm64", LocalBackend::Cuda),
            ("windows-x64", LocalBackend::Cuda),
            ("windows-x64", LocalBackend::Cpu),
        ] {
            assert!(index.build(pair.0, pair.1).is_ok(), "{pair:?}");
        }
    }

    #[test]
    fn hip_outside_x86_64_and_musl_hosts_are_unsupported() {
        let index = EngineIndex::embedded();
        for (platform, backend) in [
            ("darwin-arm64", LocalBackend::Hip),
            ("linux-arm64", LocalBackend::Hip),
            ("linux-x64-musl", LocalBackend::Cpu),
        ] {
            let err = index.build(platform, backend).unwrap_err();
            assert_eq!(err.exit_code(), 69);
            assert!(
                err.message().contains(&format!("({backend}, {platform})")),
                "{}",
                err.message()
            );
        }
    }
}
