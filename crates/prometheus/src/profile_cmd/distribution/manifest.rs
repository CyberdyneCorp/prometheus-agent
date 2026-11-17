//! `distribution.toml` and what a distribution may install (spec:
//! configuration-and-profiles, Profile distributions).
//!
//! Only known content is ever installed: the identity file, an initial
//! `config.toml`, skills (`skills/<entry>/`), MCP server definitions
//! (`mcp.toml`, `[servers.<name>]`) and cron jobs (`cron.toml`,
//! `[[jobs]]`). Everything else in the source (credential stores,
//! `state.db`, sessions, logs, snapshots, a stray `secrets.toml`) is never
//! copied, inside skill directories included.

use std::path::Path;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// The manifest file.
pub const MANIFEST: &str = "distribution.toml";
/// The identity file.
pub const IDENTITY: &str = "SOUL.md";
/// The initial configuration.
pub const CONFIG: &str = "config.toml";
/// MCP server definitions.
pub const MCP: &str = "mcp.toml";
/// Cron job definitions.
pub const CRON: &str = "cron.toml";
/// The skills directory.
pub const SKILLS: &str = "skills";

/// Names never copied from a distribution, wherever they appear.
const NEVER: &[&str] = &[
    "secrets.toml",
    "secrets.toml.lock",
    "grants.toml",
    "auth.json",
    "state.db",
    "state.db-wal",
    "state.db-shm",
    "sessions",
    "memories",
    "logs",
    "snapshots",
    "backups",
    "trajectories",
    "blobs",
    "cache",
    "run",
    ".git",
];

/// Whether a file or directory named `name` is never installed.
#[must_use]
pub fn never_installed(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    NEVER.contains(&lower.as_str())
        || Path::new(&lower)
            .extension()
            .is_some_and(|e| e == "db" || e == "key")
        || lower.starts_with(".env")
}

/// The manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The distribution's name (the default profile name).
    pub name: String,
    /// Its version.
    pub version: String,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// The Prometheus versions it works with (a semver range).
    pub prometheus_requires: String,
    /// Who wrote it.
    #[serde(default)]
    pub author: Option<String>,
    /// Its license.
    #[serde(default)]
    pub license: Option<String>,
    /// The paths an update replaces (default: the identity file, every
    /// skill entry, `mcp.toml` and `cron.toml` it ships).
    #[serde(default)]
    pub owned: Option<Vec<String>>,
    /// The secrets it needs.
    #[serde(default)]
    pub secrets_required: Vec<SecretNeed>,
}

/// One secret a distribution needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretNeed {
    /// The secret's name.
    pub name: String,
    /// What it is.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether install asks for it when missing.
    #[serde(default = "yes")]
    pub required: bool,
}

const fn yes() -> bool {
    true
}

impl Manifest {
    /// Reads `<root>/distribution.toml`.
    ///
    /// # Errors
    ///
    /// `profile.distribution_invalid` for a missing or invalid manifest.
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join(MANIFEST);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
        let manifest: Self = toml::from_str(&text)
            .map_err(|e| invalid(format!("{}: {}", path.display(), e.message())))?;
        pmt_config::ProfileName::parse(&manifest.name)
            .map_err(|e| invalid(format!("name: {}", e.message())))?;
        for need in &manifest.secrets_required {
            pmt_config::secrets::check_name(&need.name)?;
        }
        Ok(manifest)
    }

    /// Refuses a distribution this build does not satisfy.
    ///
    /// # Errors
    ///
    /// `profile.distribution_incompatible` (78).
    pub fn check_compatible(&self, version: &str) -> Result<(), Error> {
        let range = semver::VersionReq::parse(&self.prometheus_requires).map_err(|e| {
            invalid(format!(
                "prometheus_requires '{}': {e}",
                self.prometheus_requires
            ))
        })?;
        let ours = semver::Version::parse(version).map_err(|e| invalid(e.to_string()))?;
        if range.matches(&ours) {
            return Ok(());
        }
        Err(Error::new(
            &codes::PROFILE_DISTRIBUTION_INCOMPATIBLE,
            format!(
                "distribution '{}' {} needs Prometheus {}, this is {version}",
                self.name, self.version, self.prometheus_requires
            ),
        ))
    }

    /// The owned paths of a distribution rooted at `root`.
    #[must_use]
    pub fn owned_paths(&self, root: &Path) -> Vec<String> {
        if let Some(owned) = &self.owned {
            return owned.clone();
        }
        let mut paths: Vec<String> = [IDENTITY, MCP, CRON]
            .into_iter()
            .filter(|p| root.join(p).is_file())
            .map(str::to_owned)
            .collect();
        paths.extend(
            skill_entries(root)
                .into_iter()
                .map(|e| format!("{SKILLS}/{e}")),
        );
        paths
    }
}

/// The skill entries a distribution ships (`skills/<entry>/`).
#[must_use]
pub fn skill_entries(root: &Path) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(root.join(SKILLS))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !never_installed(name) && !name.starts_with('.'))
        .collect();
    entries.sort();
    entries
}

/// Files at the top of the source that are never installed (shown by the
/// preview so a leaked credential file is noticed).
#[must_use]
pub fn refused_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".git" && never_installed(name))
        .collect();
    names.sort();
    names
}

pub(super) fn invalid(message: String) -> Error {
    Error::new(&codes::PROFILE_DISTRIBUTION_INVALID, message)
}
