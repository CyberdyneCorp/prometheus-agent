//! `prometheus update --dry-run` (spec: distribution-and-update, Update
//! dry-run and changelog preview): resolves the target release and
//! verifies its signed manifest, then says what the update would do
//! without downloading the binary or changing anything: the versions and
//! download size, the changelog entries between them (from the signed
//! notes in the manifest), each profile's config and storage migrations,
//! the runtime pins that change and whether a daemon restart is needed.

use std::collections::BTreeMap;

use pmt_core::error::Error;
use serde::Serialize;

use super::check::{Checked, begin};
use super::{Session, finish, self_updating};
use crate::manifest::{ReleaseManifest, Schemas};
use crate::receipts::{Kind, Outcome};
use crate::runtimes::index::RuntimeIndex;
use crate::version::{Channel, ReleaseVersion};

/// One changelog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The release that made the change.
    pub version: ReleaseVersion,
    /// The entry.
    pub text: String,
}

/// The changelog between the two versions, grouped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    /// Breaking changes.
    pub breaking: Vec<Entry>,
    /// Features.
    pub features: Vec<Entry>,
    /// Fixes.
    pub fixes: Vec<Entry>,
    /// Security fixes.
    pub security: Vec<Entry>,
}

/// A schema change `from → to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Step {
    /// The profile's schema now.
    pub from: u32,
    /// The release's schema.
    pub to: u32,
}

/// What would migrate in one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Migration {
    /// The profile.
    pub profile: String,
    /// `config.toml` schema migration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Step>,
    /// `state.db` schema migration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<Step>,
}

/// A managed runtime whose pin changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinChange {
    /// The runtime.
    pub runtime: String,
    /// This build's pin (`None`: new in the release).
    pub from: Option<String>,
    /// The release's pin (`None`: dropped by the release).
    pub to: Option<String>,
}

/// The preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The installed version.
    pub from: ReleaseVersion,
    /// The channel's newest release.
    pub to: ReleaseVersion,
    /// The channel.
    pub channel: Channel,
    /// A newer release exists (exit 10, like `--check`).
    pub available: bool,
    /// The archive this host would download, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
    /// The changelog between the versions.
    pub changes: Changes,
    /// Per-profile migrations.
    pub migrations: Vec<Migration>,
    /// Runtime pins that change.
    pub runtimes: Vec<PinChange>,
    /// A running daemon would be restarted.
    pub daemon_restart: bool,
}

impl Plan {
    /// Exit status: 10 when an update is available, like `--check`.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.available { 10 } else { 0 }
    }
}

/// Builds the preview; `daemon_running` says whether a daemon of this
/// binary runs. Writes a `check` receipt and nothing else.
///
/// # Errors
///
/// `update.owned_elsewhere` (69), `update.signature` (77),
/// `update.failed` (1).
pub async fn dry_run(
    session: &Session<'_>,
    channel: Option<Channel>,
    daemon_running: bool,
) -> Result<Plan, Error> {
    let install = session.install;
    let receipt = self_updating(install, Kind::Check)?;
    let channel = channel.unwrap_or(receipt.channel);
    let mut writer = begin(install, Kind::Check, channel)?;
    let result = async {
        let latest = session.releases.channel_index(channel).await?.version;
        writer.update(|r| {
            r.to = Some(latest.to_string());
            r.stages.push("check".to_owned());
        })?;
        let available = matches!(
            Checked::compare(install.version, channel, latest),
            Checked::Available { .. }
        );
        let mut plan = Plan {
            from: install.version,
            to: latest,
            channel,
            available,
            download_size: None,
            changes: Changes::default(),
            migrations: Vec::new(),
            runtimes: Vec::new(),
            daemon_restart: false,
        };
        if available {
            let manifest = session.releases.manifest(&latest).await?;
            fill(&mut plan, &manifest, session, daemon_running)?;
        }
        Ok(plan)
    }
    .await;
    finish(writer, result, Outcome::Success)
}

fn fill(
    plan: &mut Plan,
    manifest: &ReleaseManifest,
    session: &Session<'_>,
    daemon_running: bool,
) -> Result<(), Error> {
    let install = session.install;
    plan.download_size = manifest.artifact_for(&install.target).map(|a| a.size);
    plan.changes = changes(manifest, plan.from, plan.to);
    if let Some(schemas) = manifest.details.schemas {
        plan.migrations = migrations(&install.home, schemas)?;
    }
    plan.runtimes = pin_changes(&RuntimeIndex::embedded(), &manifest.details.runtimes);
    plan.daemon_restart = daemon_running;
    Ok(())
}

/// The entries of releases after `from` up to and including `to`.
#[must_use]
pub fn changes(manifest: &ReleaseManifest, from: ReleaseVersion, to: ReleaseVersion) -> Changes {
    let mut out = Changes::default();
    for notes in manifest
        .details
        .notes
        .iter()
        .filter(|n| n.version > from && n.version <= to)
    {
        let entries = |list: &[String]| -> Vec<Entry> {
            list.iter()
                .map(|text| Entry {
                    version: notes.version,
                    text: text.clone(),
                })
                .collect()
        };
        out.breaking.extend(entries(&notes.breaking));
        out.features.extend(entries(&notes.features));
        out.fixes.extend(entries(&notes.fixes));
        out.security.extend(entries(&notes.security));
    }
    out
}

fn migrations(home: &pmt_config::Home, schemas: Schemas) -> Result<Vec<Migration>, Error> {
    let mut out = Vec::new();
    for name in home.profiles()? {
        let profile = home.existing_profile(&name)?;
        let older = |current: Option<u32>, to: u32| {
            current
                .filter(|from| *from < to)
                .map(|from| Step { from, to })
        };
        let config = older(profile.config_file().schema_version(), schemas.config);
        let store = older(
            pmt_store::inspect::schema_version(&profile).ok().flatten(),
            schemas.store,
        );
        if config.is_some() || store.is_some() {
            out.push(Migration {
                profile: name.to_string(),
                config,
                store,
            });
        }
    }
    Ok(out)
}

/// The runtimes whose version differs between this build's index and the
/// release's pins.
#[must_use]
pub fn pin_changes(index: &RuntimeIndex, release: &BTreeMap<String, String>) -> Vec<PinChange> {
    if release.is_empty() {
        return Vec::new();
    }
    let mut names: Vec<&str> = index.names();
    names.extend(release.keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let from = index.pin(name).ok().map(|p| p.version.clone());
            let to = release.get(name).cloned();
            (from != to).then(|| PinChange {
                runtime: name.to_owned(),
                from,
                to,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{changes, pin_changes};
    use crate::manifest::{MANIFEST_SCHEMA, ReleaseDetails, ReleaseManifest, ReleaseNotes};
    use crate::runtimes::index::RuntimeIndex;
    use crate::version::ReleaseVersion;

    fn v(text: &str) -> ReleaseVersion {
        ReleaseVersion::parse(text).unwrap()
    }

    fn notes(version: &str, breaking: &[&str], fixes: &[&str]) -> ReleaseNotes {
        ReleaseNotes {
            version: v(version),
            breaking: breaking.iter().map(|s| (*s).to_owned()).collect(),
            features: Vec::new(),
            fixes: fixes.iter().map(|s| (*s).to_owned()).collect(),
            security: Vec::new(),
        }
    }

    #[test]
    fn only_entries_after_the_installed_version_are_listed() {
        let manifest = ReleaseManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            version: v("v2.0.0"),
            commit: "c".to_owned(),
            published_at: "2026-11-01T00:00:00Z".to_owned(),
            artifacts: Vec::new(),
            details: ReleaseDetails {
                notes: vec![
                    notes("v2.0.0", &["config v1 to v2"], &[]),
                    notes("v1.4.2", &[], &["a fix"]),
                    notes("v1.4.1", &[], &["already installed"]),
                ],
                ..ReleaseDetails::default()
            },
        };
        let c = changes(&manifest, v("v1.4.1"), v("v2.0.0"));
        assert_eq!(c.breaking.len(), 1);
        assert_eq!(c.breaking[0].version, v("v2.0.0"));
        assert_eq!(c.fixes.len(), 1);
        assert_eq!(c.fixes[0].text, "a fix");
    }

    #[test]
    fn changed_runtime_pins_are_listed() {
        let index = RuntimeIndex::embedded();
        let node = index.pin("node").unwrap().version.clone();
        let mut release: BTreeMap<String, String> = index
            .names()
            .into_iter()
            .map(|n| (n.to_owned(), index.pin(n).unwrap().version.clone()))
            .collect();
        release.insert("node".to_owned(), "99.0.0".to_owned());
        let changed = pin_changes(&index, &release);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].runtime, "node");
        assert_eq!(changed[0].from.as_deref(), Some(node.as_str()));
        assert_eq!(changed[0].to.as_deref(), Some("99.0.0"));
    }
}
