//! Hermes plugins in a migrated home (spec: python-bridge, Migration of
//! Hermes plugins). `prometheus migrate from-hermes` (hermes-migration)
//! calls [`import`] with the Hermes plugin directory and the plugins Hermes
//! had enabled: each plugin is copied into the profile's `plugins/` as a
//! python plugin with an `adopted` install record and audited. Every
//! plugin arrives disabled with no grants (hermes-migration, MCP servers
//! and plugins): the report says which were `compatible` or `degraded`
//! (and enabled in Hermes) and proposes their inferred capabilities for
//! approval at first enable; `incompatible` plugins are reported with the
//! APIs that block them.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::manifest::{HERMES_MANIFEST_FILE, describe};
use pmt_plugins::python::audit::{self, Verdict};
use pmt_plugins::record::{Flags, InstallRecord, Origin};
use serde::Serialize;

/// One imported plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Imported {
    /// Its id (the Hermes directory name).
    pub id: String,
    /// The audit's verdict.
    pub verdict: Verdict,
    /// Hermes had it enabled (it arrives disabled all the same).
    pub was_enabled: bool,
    /// Capabilities to approve at first enable (plain language).
    pub proposed: Vec<String>,
    /// The APIs that make it incompatible.
    pub blocking: Vec<String>,
    /// Why it was skipped (a plugin of that id already exists).
    pub skipped: Option<String>,
}

/// The plugins part of a migration report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Every plugin found, by id.
    pub plugins: Vec<Imported>,
}

impl Report {
    /// The `incompatible` plugins with their blocking APIs, as the
    /// migration report lists them.
    #[must_use]
    pub fn incompatible_lines(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|p| p.verdict == Verdict::Incompatible)
            .map(|p| {
                format!(
                    "{}: incompatible ({}); imported disabled",
                    p.id,
                    p.blocking.join(", ")
                )
            })
            .collect()
    }
}

/// Imports every plugin directory of `source` (a Hermes `plugins/`
/// directory) but those named in `skip`; `enabled` are the plugins Hermes
/// had enabled.
///
/// # Errors
///
/// When the source cannot be read or the profile cannot be written.
pub fn import(
    scope: &ProfileScope,
    source: &Path,
    enabled: &[String],
    skip: &[String],
) -> Result<Report, Error> {
    let mut report = Report::default();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(source)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, source, &e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(HERMES_MANIFEST_FILE).is_file())
        .filter(|p| {
            p.file_name()
                .is_none_or(|n| !skip.iter().any(|s| n == s.as_str()))
        })
        .collect();
    dirs.sort();
    for dir in dirs {
        report.plugins.push(one(scope, &dir, enabled)?);
    }
    Ok(report)
}

fn one(scope: &ProfileScope, dir: &Path, enabled: &[String]) -> Result<Imported, Error> {
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = scope.dir().join("plugins").join(&id);
    let mut imported = Imported {
        id: id.clone(),
        verdict: Verdict::Compatible,
        was_enabled: enabled.contains(&id),
        proposed: Vec::new(),
        blocking: Vec::new(),
        skipped: None,
    };
    if target.exists() {
        imported.skipped = Some(format!("a plugin '{id}' already exists in the profile"));
        return Ok(imported);
    }
    copy(dir, &target)?;
    let manifest = pmt_plugins::python::hermes::read(&target)?;
    let report = audit::audit(&target, &manifest.capabilities.hooks)?;
    imported.verdict = report.verdict;
    imported.blocking = report.blocking_names();
    let data = scope.dir().join("plugin-data").join(&id);
    imported.proposed = manifest
        .capabilities
        .requested(&data)
        .map(|set| set.iter().map(describe).collect())
        .unwrap_or_default();
    InstallRecord {
        source: Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&target)?,
        signer: None,
        tier: None,
        installed_at: pmt_plugins::install::now(),
        flags: Flags {
            forced: report.verdict == Verdict::Incompatible,
            ..Flags::default()
        },
        held_update: None,
    }
    .write(&target)?;
    Ok(imported)
}

fn copy(from: &Path, to: &Path) -> Result<(), Error> {
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::PLUGINS_DISCOVERY, path, &e);
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| io(from, e))?.flatten() {
        let name = entry.file_name();
        if name == "__pycache__" || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type().map_err(|e| io(&entry.path(), e))?;
        if kind.is_dir() {
            copy(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).map_err(|e| io(&target, e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    use super::*;

    #[test]
    fn an_incompatible_plugin_is_imported_disabled_and_cited() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let hermes = root.path().join("hermes-plugins");
        for name in ["platform", "spotify", "security-guidance"] {
            let to = hermes.join(name);
            copy(&pmt_pybridge::testkit::fixture(name), &to).unwrap();
        }
        let enabled = ["platform".to_owned(), "spotify".to_owned()];
        let report = import(&scope, &hermes, &enabled, &[]).unwrap();
        let by_id = |id: &str| report.plugins.iter().find(|p| p.id == id).unwrap().clone();
        let platform = by_id("platform");
        assert_eq!(platform.verdict, Verdict::Incompatible);
        assert!(platform.was_enabled);
        assert_eq!(platform.blocking, ["register_platform"]);
        assert!(scope.dir().join("plugins/platform/__init__.py").is_file());
        assert!(report.incompatible_lines()[0].contains("register_platform"));
        let spotify = by_id("spotify");
        assert_eq!(spotify.verdict, Verdict::Compatible);
        assert!(
            spotify.proposed.iter().any(|p| p.contains("SPOTIFY_TOKEN")),
            "{:?}",
            spotify.proposed
        );
        assert!(
            !by_id("security-guidance").was_enabled,
            "not enabled in Hermes"
        );
        // Nothing is enabled, even what Hermes had enabled.
        let config = std::fs::read_to_string(scope.dir().join("config.toml")).unwrap();
        assert!(!config.contains("spotify"), "{config}");
        // Proposed, not granted: the plugin waits for approval.
        let grants = std::fs::read_to_string(scope.dir().join("grants.toml")).unwrap_or_default();
        assert!(!grants.contains("spotify"), "{grants}");
    }
}
