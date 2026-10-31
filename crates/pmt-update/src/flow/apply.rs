//! `prometheus update` (spec: distribution-and-update, Atomic binary swap
//! with health check; Schema migrations around updates): under the update
//! lock, download and verify the archive into
//! `<home>/updates/staging/<receipt id>/`, run the staged build's
//! self-test, snapshot every profile (reason `pre-update`, label
//! `pre-<version>`) so a rollback can restore the state the old build
//! reads, then swap the binary atomically.

use std::path::{Path, PathBuf};

use pmt_backup::snapshot::{self, Reason};
use pmt_config::ProfileDir;
use pmt_core::error::{Error, codes};

use super::check::begin;
use super::{Checked, Install, Session, finish, refuse, self_updating};
use crate::archive;
use crate::install::InstallReceipt;
use crate::lock::UpdateLock;
use crate::manifest::ReleaseManifest;
use crate::probe;
use crate::receipts::{Kind, Outcome, ReceiptWriter, SnapshotRef};
use crate::swap;
use crate::trust;
use crate::version::{Channel, ReleaseVersion};

/// Options of one `prometheus update` run.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// `--channel`: this run only.
    pub channel: Option<Channel>,
    /// `--allow-downgrade`: install the channel's newest even when older.
    pub allow_downgrade: bool,
}

/// What `update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updated {
    /// The binary was replaced.
    Installed {
        /// Previous version.
        from: ReleaseVersion,
        /// Installed version.
        to: ReleaseVersion,
        /// Pre-update snapshots, one per profile.
        snapshots: Vec<SnapshotRef>,
    },
    /// Nothing to install (current, or ahead of the channel).
    Unchanged(Checked),
}

impl Updated {
    /// The one-line result.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Installed { from, to, .. } => format!("updated prometheus {from} → {to}"),
            Self::Unchanged(checked) => checked.message(),
        }
    }
}

/// Runs `prometheus update`. Exit 0 on `Ok` (updated or already current).
///
/// # Errors
///
/// `update.owned_elsewhere` (69), `update.locked` (75), `update.signature`
/// (77), `update.self_test_failed` / `update.failed` (1); the installed
/// binary is unchanged on every error.
pub async fn update(session: &Session<'_>, options: UpdateOptions) -> Result<Updated, Error> {
    let install = session.install;
    let receipt = self_updating(install, Kind::Update)?;
    let channel = options.channel.unwrap_or(receipt.channel);
    let _lock = UpdateLock::acquire(install.home.root(), &install.binary).inspect_err(|err| {
        let _ = refuse(install, Kind::Update, channel.as_str(), err);
    })?;
    let mut writer = begin(install, Kind::Update, channel)?;
    let result = run(session, &mut writer, receipt, channel, options).await;
    let staging = staging_dir(install, &writer);
    let _ = std::fs::remove_dir_all(staging);
    finish(writer, result, Outcome::Success)
}

async fn run(
    session: &Session<'_>,
    writer: &mut ReceiptWriter,
    mut receipt: InstallReceipt,
    channel: Channel,
    options: UpdateOptions,
) -> Result<Updated, Error> {
    let install = session.install;
    let say = session.progress;
    say(&format!("checking the {channel} channel"));
    let latest = session.releases.channel_index(channel).await?.version;
    writer.update(|r| {
        r.to = Some(latest.to_string());
        r.stages.push("check".to_owned());
    })?;
    let checked = Checked::compare(install.version, channel, latest);
    let wanted = match checked {
        Checked::Available { to, .. } => to,
        Checked::Ahead { latest, .. } if options.allow_downgrade => latest,
        other => return Ok(Updated::Unchanged(other)),
    };
    let manifest = session.releases.manifest(&wanted).await?;
    let staged = download(session, writer, &manifest).await?;
    say("running the staged build's self-test");
    probe::self_test(&staged, install.home.root())?;
    writer.stage("self-test")?;
    let snapshots = snapshot_profiles(install, session.config.snapshot_keep, &wanted)?;
    writer.update(|r| {
        r.snapshots.clone_from(&snapshots);
        r.stages.push("snapshot".to_owned());
    })?;
    say("swapping the binary");
    swap::install(&staged, &install.binary)?;
    writer.stage("swap")?;
    receipt.version = wanted.to_string();
    receipt.write(install.home.root())?;
    Ok(Updated::Installed {
        from: install.version,
        to: wanted,
        snapshots,
    })
}

/// Downloads, verifies and unpacks this host's archive; returns the
/// staged binary.
async fn download(
    session: &Session<'_>,
    writer: &mut ReceiptWriter,
    manifest: &ReleaseManifest,
) -> Result<PathBuf, Error> {
    let install = session.install;
    let artifact = manifest.artifact_for(&install.target).ok_or_else(|| {
        Error::new(
            &codes::UPDATE_FAILED,
            format!(
                "release {} has no archive for {}",
                manifest.version, install.target
            ),
        )
    })?;
    (session.progress)(&format!(
        "downloading {} ({} bytes)",
        artifact.name, artifact.size
    ));
    let path = format!("{}/{}", manifest.version, artifact.name);
    let bytes = session.releases.verified(&path).await?;
    writer.stage("download")?;
    if !artifact.matches(&bytes) {
        return Err(trust::failed(&artifact.name));
    }
    writer.stage("verify")?;
    let staged = archive::extract_binary(&bytes, &staging_dir(install, writer))?;
    writer.stage("unpack")?;
    Ok(staged)
}

/// Takes a `pre-update` snapshot of every profile (config and a
/// consistent copy of `state.db`, among the quick member set) and keeps the
/// newest `keep` of them per profile.
fn snapshot_profiles(
    install: &Install,
    keep: u32,
    to: &ReleaseVersion,
) -> Result<Vec<SnapshotRef>, Error> {
    let label = format!("pre-{to}");
    let mut taken = Vec::new();
    for name in install.home.profiles()? {
        let profile = install.home.existing_profile(&name)?;
        let info = snapshot::create(&profile, Reason::PreUpdate, Some(&label))?;
        prune_pre_update(&profile, keep)?;
        taken.push(SnapshotRef {
            profile: name.to_string(),
            id: info.id,
            created_at: info.created_at,
        });
    }
    Ok(taken)
}

fn prune_pre_update(profile: &ProfileDir, keep: u32) -> Result<(), Error> {
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    snapshot::prune_reason(profile, Reason::PreUpdate, keep).map(drop)
}

fn staging_dir(install: &Install, writer: &ReceiptWriter) -> PathBuf {
    staging_root(install.home.root()).join(&writer.receipt().id)
}

/// `<home>/updates/staging`.
fn staging_root(home: &Path) -> PathBuf {
    home.join("updates").join("staging")
}
