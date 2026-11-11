//! `prometheus update` (spec: distribution-and-update, Atomic binary swap
//! with health check; Schema migrations around updates; Pre-update backup;
//! Runtime leases and garbage collection): under the update lock, download
//! and verify the archive into `<home>/updates/staging/<receipt id>/`, run
//! the staged build's self-test, take the pre-update backup (see
//! [`super::backup`]) so a rollback can restore the state the old build
//! reads, swap the binary atomically, then let the new build refresh its
//! runtime pins and collect unpinned runtimes.

use std::path::{Path, PathBuf};

use pmt_config::daemon::PreUpdateBackup;
use pmt_core::error::{Error, codes};

use super::backup::{self, Step};
use super::check::begin;
use super::handover::{Daemon, HandedOver, RESTART_PENDING};
use super::{Checked, Install, Session, finish, refuse, self_updating};
use crate::archive;
use crate::install::{InstallReceipt, Owner};
use crate::lock::UpdateLock;
use crate::manifest::ReleaseManifest;
use crate::probe;
use crate::receipts::{BackupRef, Kind, Outcome, ReceiptWriter, SnapshotRef};
use crate::swap;
use crate::trust;
use crate::version::{Channel, ReleaseVersion};

/// Options of one `prometheus update` run.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions<'a> {
    /// `--channel`: this run only.
    pub channel: Option<Channel>,
    /// `--allow-downgrade`: install the channel's newest even when older.
    pub allow_downgrade: bool,
    /// `--backup` (`full`) / `--no-backup` (`off`): overrides
    /// `update.pre_update_backup` for this run.
    pub backup: Option<PreUpdateBackup>,
    /// The running daemon to hand over to the new build.
    pub daemon: Option<&'a dyn Daemon>,
    /// `--from-bundle`: the unpacked bundle the session's releases read;
    /// the receipt records `source: bundle`, the install becomes
    /// `owner = bundle`, and runtimes refresh from it.
    pub bundle: Option<&'a Path>,
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
        /// Every pre-update backup with its size.
        backups: Vec<BackupRef>,
        /// What happened to the running daemon.
        daemon: HandedOver,
    },
    /// Nothing to install (current, or ahead of the channel).
    Unchanged(Checked),
}

impl Updated {
    /// The one-line result.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Installed {
                from, to, daemon, ..
            } => {
                let updated = format!("updated prometheus {from} → {to}");
                match daemon {
                    HandedOver::NotRunning => updated,
                    HandedOver::Restarted(version) => {
                        format!("{updated}\ndaemon restarted on {version}")
                    }
                    HandedOver::Pending { reason, command } => format!(
                        "{updated}\nthe daemon still runs {from} ({reason}); restart it with: {command}"
                    ),
                }
            }
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
pub async fn update(session: &Session<'_>, options: UpdateOptions<'_>) -> Result<Updated, Error> {
    let install = session.install;
    let receipt = self_updating(install, Kind::Update)?;
    let channel = options.channel.unwrap_or(receipt.channel);
    let _lock = UpdateLock::acquire(install.home.root(), &install.binary).inspect_err(|err| {
        let _ = refuse(install, Kind::Update, channel.as_str(), err);
    })?;
    let mut writer = begin(install, Kind::Update, channel)?;
    if options.bundle.is_some() {
        writer.update(|r| r.source = Some("bundle".to_owned()))?;
    }
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
    options: UpdateOptions<'_>,
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
    say("self-test of the staged build");
    let staged_info = probe::self_test(&staged, install.home.root())?;
    writer.stage("self-test")?;
    let mode = options.backup.unwrap_or(session.config.pre_update_backup);
    say(&format!("pre-update backup ({})", mode.as_str()));
    let taken = backup::run(&Step {
        install,
        mode,
        snapshot_keep: session.config.snapshot_keep,
        staged: &staged_info,
        to: &wanted,
    })?;
    writer.update(|r| {
        r.snapshots.clone_from(&taken.snapshots);
        r.backups.clone_from(&taken.backups);
        r.stages.push("backup".to_owned());
    })?;
    if let Some(daemon) = options.daemon {
        daemon.prepare()?;
    }
    say("swapping the binary");
    swap::install(&staged, &install.binary)?;
    writer.stage("swap")?;
    receipt.version = wanted.to_string();
    if options.bundle.is_some() {
        receipt.owner = Owner::Bundle;
    }
    receipt.write(install.home.root())?;
    refresh_runtimes(install, writer, say, options.bundle)?;
    let daemon = match options.daemon {
        Some(daemon) => hand_over(daemon, writer, &wanted, say)?,
        None => HandedOver::NotRunning,
    };
    Ok(Updated::Installed {
        from: install.version,
        to: wanted,
        snapshots: taken.snapshots,
        backups: taken.backups,
        daemon,
    })
}

/// Restarts the running daemon on the new build, or records that it
/// still runs the old one; the update has succeeded either way.
fn hand_over(
    daemon: &dyn Daemon,
    writer: &mut ReceiptWriter,
    to: &ReleaseVersion,
    say: &dyn Fn(&str),
) -> Result<HandedOver, Error> {
    say("restarting the daemon");
    let outcome = daemon.hand_over(to);
    match &outcome {
        HandedOver::NotRunning => {}
        HandedOver::Restarted(_) => writer.stage("restart")?,
        HandedOver::Pending { .. } => {
            writer.update(|r| r.follow_ups.push(RESTART_PENDING.to_owned()))?;
        }
    }
    Ok(outcome)
}

/// Lets the new build install its runtime pins for the runtimes in use
/// (never an excluded one) and collect versions neither build pins. The
/// update itself is done: a failure here is a follow-up, not a failure.
fn refresh_runtimes(
    install: &Install,
    writer: &mut ReceiptWriter,
    say: &dyn Fn(&str),
    bundle: Option<&Path>,
) -> Result<(), Error> {
    say("refreshing managed runtimes");
    match probe::refresh_runtimes(&install.binary, install.home.root(), bundle) {
        Ok(()) => writer.stage("runtimes"),
        Err(reason) => {
            say(&format!("runtime refresh failed: {reason}"));
            writer.update(|r| {
                r.follow_ups.push(format!(
                    "runtime refresh failed ({reason}): run 'prometheus runtimes gc' and reinstall what tools need with 'prometheus runtimes install <name>'"
                ));
            })
        }
    }
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
    let bytes = session
        .releases
        .verified_with_progress(&path, artifact.size, session.progress)
        .await?;
    writer.stage("download")?;
    (session.progress)(&format!("verifying {}", artifact.name));
    if !artifact.matches(&bytes) {
        return Err(trust::failed(&artifact.name));
    }
    writer.stage("verify")?;
    let staged = archive::extract_binary(&bytes, &staging_dir(install, writer))?;
    writer.stage("unpack")?;
    Ok(staged)
}

fn staging_dir(install: &Install, writer: &ReceiptWriter) -> PathBuf {
    staging_root(install.home.root()).join(&writer.receipt().id)
}

/// `<home>/updates/staging`.
fn staging_root(home: &Path) -> PathBuf {
    home.join("updates").join("staging")
}
