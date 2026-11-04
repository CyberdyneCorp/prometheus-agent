//! The supply-chain advisory catalog (spec: security-boundaries,
//! Supply-chain advisory catalog): known-compromised package versions.
//!
//! One catalog is embedded in each release (it is covered by the release's
//! own signature). The update check also fetches `channels/advisories.json` and its
//! minisign signature from the release origin, verifies them against the
//! embedded release keys and keeps them under `<home>/updates/`; at load
//! the kept copy is verified again and used when it is newer than the
//! embedded one. A copy that does not verify is ignored with a WARN.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::fetch::Releases;
use crate::trust::TrustRoot;

/// `schema` of a catalog.
pub const SCHEMA: &str = "prometheus.advisories.v1";
/// The catalog's file name (in `<home>/updates/` and on the origin).
pub const FILE: &str = "advisories.json";
/// Its path below the release origin, beside the channel indexes.
pub const ORIGIN_PATH: &str = "channels/advisories.json";
/// The detached signature's suffix.
const SIGNATURE_SUFFIX: &str = ".minisig";

const EMBEDDED: &str = include_str!("../advisories/advisories.json");

/// Where a package comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    /// npm (MCP servers run with `npx`/`bunx`).
    Npm,
    /// `PyPI` (python-bridge environments, `uvx` MCP servers).
    Pypi,
    /// An installed Prometheus plugin, by id.
    Plugin,
    /// A managed runtime, by name.
    Runtime,
}

impl Ecosystem {
    /// The serialized name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Plugin => "plugin",
            Self::Runtime => "runtime",
        }
    }
}

/// One advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    /// Stable id (`PMT-ADV-2024-0001`).
    pub id: String,
    /// The package's ecosystem.
    pub ecosystem: Ecosystem,
    /// The package name (case-insensitive; `PyPI` names also ignore `-`,
    /// `_` and `.` differences).
    pub package: String,
    /// The exact compromised versions.
    pub versions: Vec<String>,
    /// What happened.
    pub summary: String,
    /// What to do.
    pub remediation: String,
}

/// A catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// [`SCHEMA`].
    pub schema: String,
    /// When it was generated (`YYYY-MM-DD`); the newer catalog wins.
    pub generated: String,
    /// The advisories.
    pub advisories: Vec<Advisory>,
}

/// An installed package version, as collected from the home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its ecosystem.
    pub ecosystem: Ecosystem,
    /// Its name.
    pub name: String,
    /// Its exact version.
    pub version: String,
    /// Where it was found (`plugin weather`, `mcp server github`...).
    pub location: String,
}

/// An installed version an advisory lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The advisory.
    pub advisory: Advisory,
    /// What matched.
    pub installed: Installed,
}

impl Catalog {
    /// Parses a catalog.
    ///
    /// # Errors
    ///
    /// `update.failed` for malformed JSON or another schema.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let catalog: Self = serde_json::from_str(text)
            .map_err(|e| Error::new(&codes::UPDATE_FAILED, format!("advisory catalog: {e}")))?;
        if catalog.schema != SCHEMA {
            return Err(Error::new(
                &codes::UPDATE_FAILED,
                format!("advisory catalog: unknown schema '{}'", catalog.schema),
            ));
        }
        Ok(catalog)
    }

    /// The catalog embedded in this binary.
    ///
    /// # Panics
    ///
    /// Never for the committed file (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("the embedded advisory catalog parses")
    }

    /// The catalog in force for `home`: the verified copy the update
    /// check kept when it is newer than the embedded one.
    #[must_use]
    pub fn current(home: &Path, trust: &TrustRoot) -> Self {
        let embedded = Self::embedded();
        match kept(home, trust) {
            Ok(Some(kept)) if kept.generated > embedded.generated => kept,
            Ok(_) => embedded,
            Err(err) => {
                tracing::warn!("ignoring the kept advisory catalog: {}", err.message());
                embedded
            }
        }
    }

    /// The advisory of `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Advisory> {
        self.advisories
            .iter()
            .find(|a| a.id.eq_ignore_ascii_case(id))
    }

    /// Every installed version an advisory lists.
    #[must_use]
    pub fn matches(&self, installed: &[Installed]) -> Vec<Match> {
        let mut found = Vec::new();
        for item in installed {
            for advisory in &self.advisories {
                if advisory.ecosystem == item.ecosystem
                    && same_name(item.ecosystem, &advisory.package, &item.name)
                    && advisory.versions.iter().any(|v| v == &item.version)
                {
                    found.push(Match {
                        advisory: advisory.clone(),
                        installed: item.clone(),
                    });
                }
            }
        }
        found
    }
}

fn same_name(ecosystem: Ecosystem, a: &str, b: &str) -> bool {
    let normal = |name: &str| {
        let lower = name.to_ascii_lowercase();
        if ecosystem == Ecosystem::Pypi {
            lower.replace(['_', '.'], "-")
        } else {
            lower
        }
    };
    normal(a) == normal(b)
}

/// `<home>/updates/advisories.json`.
#[must_use]
pub fn kept_path(home: &Path) -> PathBuf {
    home.join("updates").join(FILE)
}

/// The kept catalog, verified; `None` when there is none.
fn kept(home: &Path, trust: &TrustRoot) -> Result<Option<Catalog>, Error> {
    let path = kept_path(home);
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io(&codes::UPDATE_FAILED, &path, &err)),
    };
    let signature =
        std::fs::read_to_string(signature_path(&path)).map_err(|_| crate::trust::failed(FILE))?;
    trust.verify(FILE, &data, &signature)?;
    Catalog::parse(&String::from_utf8_lossy(&data)).map(Some)
}

fn signature_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(SIGNATURE_SUFFIX);
    PathBuf::from(name)
}

/// Fetches the origin's signed catalog and keeps it for `home` (with its
/// signature, so every load verifies it again).
///
/// # Errors
///
/// `update.failed` (download, malformed, write), `update.signature` (77).
pub async fn refresh(releases: &Releases, home: &Path) -> Result<Catalog, Error> {
    let (data, signature) = releases.verified_with_signature(ORIGIN_PATH).await?;
    let catalog = Catalog::parse(&String::from_utf8_lossy(&data))?;
    let path = kept_path(home);
    if let Some(dir) = path.parent() {
        crate::install::create_dirs(dir)?;
    }
    let io = |p: &Path, e: std::io::Error| Error::io(&codes::UPDATE_FAILED, p, &e);
    let signature_file = signature_path(&path);
    private_fs::write_atomic(&signature_file, signature.as_bytes())
        .map_err(|e| io(&signature_file, e))?;
    private_fs::write_atomic(&path, &data).map_err(|e| io(&path, e))?;
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Ecosystem, Installed};

    fn installed(ecosystem: Ecosystem, name: &str, version: &str) -> Installed {
        Installed {
            ecosystem,
            name: name.into(),
            version: version.into(),
            location: "test".into(),
        }
    }

    #[test]
    fn the_embedded_catalog_parses_with_unique_ids() {
        let catalog = Catalog::embedded();
        assert!(!catalog.advisories.is_empty());
        let mut ids: Vec<&str> = catalog.advisories.iter().map(|a| a.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), catalog.advisories.len());
    }

    #[test]
    fn only_listed_versions_of_the_same_package_match() {
        let catalog = Catalog::embedded();
        let found = catalog.matches(&[
            installed(Ecosystem::Npm, "UA-Parser-JS", "0.7.29"),
            installed(Ecosystem::Npm, "ua-parser-js", "0.7.30"),
            installed(Ecosystem::Pypi, "Ultralytics", "8.3.41"),
            installed(Ecosystem::Pypi, "ua-parser-js", "0.7.29"),
        ]);
        let ids: Vec<&str> = found.iter().map(|m| m.advisory.id.as_str()).collect();
        assert_eq!(ids, ["PMT-ADV-2021-0001", "PMT-ADV-2024-0002"]);
    }
}
