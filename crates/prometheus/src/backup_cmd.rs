//! `prometheus backup` and `prometheus snapshots` (spec: configuration-
//! and-profiles, Whole-home backup, Home restore, State snapshots).

use pmt_backup::snapshot::{self, Reason, SnapshotInfo};
use pmt_backup::{CreateOptions, restore};
use pmt_config::{ProfileDir, ProfileName};
use pmt_core::error::Error;
use serde::Serialize;

use crate::cli::{BackupCommand, SnapshotsCommand};
use crate::context::Context;
use crate::output::{document, print, print_raw};
use crate::prompt;

/// Exit status of a backup that skipped unreadable members.
pub const PARTIAL: u8 = 5;

/// Runs one `backup` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn backup(command: &BackupCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        BackupCommand::Create {
            output,
            quick,
            label,
            keep,
        } => {
            let options = CreateOptions {
                output: output.clone(),
                quick: *quick,
                label: label.clone(),
                keep: *keep,
            };
            create(ctx, &options)
        }
        BackupCommand::Restore { archive, force } => restore_home(ctx, archive, *force),
    }
}

fn create(ctx: &Context<'_>, options: &CreateOptions) -> Result<u8, Error> {
    let created = pmt_backup::create(&ctx.home()?, options)?;
    for member in &created.skipped {
        eprintln!("warning: skipped unreadable {member}");
    }
    let code = if created.skipped.is_empty() {
        0
    } else {
        PARTIAL
    };
    if ctx.json {
        document("backup create", &created)?;
    } else {
        eprintln!(
            "backed up {} files ({} bytes)",
            created.members, created.bytes
        );
        print(&created.path.display())?;
    }
    Ok(code)
}

fn restore_home(ctx: &Context<'_>, archive: &std::path::Path, force: bool) -> Result<u8, Error> {
    let home = ctx.home()?;
    let profile = ctx.profile.map(ProfileName::parse).transpose()?;
    let plan = restore::plan(&home, archive, profile.as_ref())?;
    for warning in &plan.warnings {
        eprintln!("warning: {warning}");
    }
    if plan.holds_data && !force {
        let what = format!(
            "restoring {} replaces existing data in {}",
            archive.display(),
            home.root().display()
        );
        match prompt::confirm_typed(&what, "restore")? {
            None => {
                eprintln!("error: the home already holds data; rerun with --force to replace it");
                return Ok(prompt::NEEDS_CONFIRMATION);
            }
            Some(false) => {
                eprintln!("nothing restored");
                return Ok(1);
            }
            Some(true) => {}
        }
    }
    let restored = plan.apply()?;
    if ctx.json {
        return document("backup restore", &restored);
    }
    for taken in &restored.snapshots {
        eprintln!("snapshot {} keeps the replaced state", taken.id);
    }
    eprintln!("restored {} files", restored.members);
    Ok(0)
}

/// Runs one `snapshots` subcommand on the selected profile.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn snapshots(command: &SnapshotsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        SnapshotsCommand::List => {
            let list = snapshot::list(&ctx.profile_dir()?)?;
            if ctx.json {
                return listing(ctx, list);
            }
            print_raw(&render_list(&list))
        }
        SnapshotsCommand::Create { label } => {
            let (taken, _) = create_manual(ctx, label.as_deref())?;
            print(&taken.id)
        }
        SnapshotsCommand::Restore { id } => {
            let restored = snapshot::restore(&ctx.profile_dir()?, id)?;
            eprintln!(
                "restored {id}; the replaced state is snapshot {}",
                restored.pre_restore.id
            );
            Ok(0)
        }
        SnapshotsCommand::Prune { keep } => {
            let keep = match keep {
                Some(keep) => *keep,
                None => configured_keep(ctx)?,
            };
            for id in snapshot::prune(&ctx.profile_dir()?, keep)? {
                eprintln!("removed {id}");
            }
            Ok(0)
        }
    }
}

fn listing(ctx: &Context<'_>, snapshots: Vec<SnapshotInfo>) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Listing {
        profile: String,
        snapshots: Vec<SnapshotInfo>,
    }
    let profile = ctx.profile_dir()?.name().to_string();
    document("snapshots list", &Listing { profile, snapshots })
}

/// One line per snapshot: `id  reason  created_at  label`.
#[must_use]
pub fn render_list(snapshots: &[SnapshotInfo]) -> String {
    snapshots
        .iter()
        .map(|s| {
            let label = s.label.as_deref().unwrap_or("");
            format!("{}  {}  {}  {label}", s.id, s.reason.as_str(), s.created_at)
                .trim_end()
                .to_owned()
                + "\n"
        })
        .collect()
}

fn configured_keep(ctx: &Context<'_>) -> Result<usize, Error> {
    let keep = ctx.scope()?.config().snapshots.keep;
    Ok(usize::try_from(keep).unwrap_or(usize::MAX))
}

/// A manual snapshot of the selected profile, then pruning to
/// `snapshots.keep`; returns the snapshot and the removed ids.
fn create_manual(
    ctx: &Context<'_>,
    label: Option<&str>,
) -> Result<(SnapshotInfo, Vec<String>), Error> {
    let keep = configured_keep(ctx)?;
    let profile = ctx.profile_dir()?;
    manual_snapshot(&profile, label, keep)
}

/// Takes a manual snapshot of `profile` and prunes to `keep`.
///
/// # Errors
///
/// Snapshot errors.
pub fn manual_snapshot(
    profile: &ProfileDir,
    label: Option<&str>,
    keep: usize,
) -> Result<(SnapshotInfo, Vec<String>), Error> {
    let taken = snapshot::create(profile, Reason::Manual, label)?;
    let pruned = snapshot::prune(profile, keep)?;
    Ok((taken, pruned))
}
