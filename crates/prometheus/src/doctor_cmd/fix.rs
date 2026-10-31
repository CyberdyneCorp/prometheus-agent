//! `prometheus doctor --fix`: exactly the enumerated repairs, each printed
//! (spec: workspace-architecture, Doctor repairs are explicit and
//! enumerated): tighten home, secrets and database permissions; remove
//! stale `run/` lock and discovery files; rebuild the full-text index;
//! apply pending config migrations after a `config.toml.bak.<timestamp>`
//! backup; reinstall managed runtimes whose hash does not match (none are
//! installed by this build). Everything else is left to the hints.

use std::path::Path;

use pmt_config::ConfigFile;
use pmt_core::error::{Error, codes};
use pmt_core::runtime::{LOCK_FILE, Liveness, Role, read_record, run_dir};

use super::groups::{loose_mode, private_paths};
use crate::context::Context;

/// The pending migration's source version, when `config.toml` is older
/// than this build.
pub(super) fn pending_migration(file: &ConfigFile) -> Option<u32> {
    let raw = file.raw().ok()??;
    let version = raw
        .lines()
        .find_map(|l| l.trim().strip_prefix("schema_version"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .and_then(|v| v.trim().parse::<u32>().ok())?;
    (version < pmt_config::schema::SCHEMA_VERSION).then_some(version)
}

/// Runs the repairs; returns one line per repair performed.
///
/// # Errors
///
/// I/O errors of a repair.
pub(super) fn run(ctx: &Context<'_>, root: &Path, profile: &Path) -> Result<Vec<String>, Error> {
    let mut done = tighten(root, profile)?;
    done.extend(stale_records(root)?);
    if let Ok(scope) = ctx.scope()
        && let Ok(store) = pmt_store::Store::open(&scope)
    {
        done.extend(store.doctor_fix()?);
    }
    if let Ok(dir) = ctx.profile_dir() {
        done.extend(migrate(&dir.config_file())?);
    }
    Ok(done)
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::new(&codes::HOME_IO, format!("{}: {e}", path.display()))
}

/// Owner-only modes on the home's directories and the profile's files.
fn tighten(root: &Path, profile: &Path) -> Result<Vec<String>, Error> {
    let (dirs, files) = private_paths(root, profile);
    let mut done = Vec::new();
    for (paths, mode) in [(dirs, 0o700), (files, 0o600)] {
        for path in paths.iter().filter(|p| p.exists()) {
            if let Some(old) = loose_mode(path) {
                set_mode(path, mode)?;
                done.push(format!(
                    "tightened {} from {old:o} to {mode:o}",
                    path.display()
                ));
            }
        }
    }
    Ok(done)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io(path, &e))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), Error> {
    Ok(())
}

/// Removes `run/` records whose process is gone, then the gateway lock
/// when no live process remains.
fn stale_records(root: &Path) -> Result<Vec<String>, Error> {
    let run = run_dir(root);
    let mut done = Vec::new();
    for (role, what) in [(Role::Daemon, "gateway"), (Role::Chat, "chat")] {
        let Some(record) = read_record(root, role)? else {
            continue;
        };
        if record.liveness() == Liveness::Alive {
            continue;
        }
        let record_path = run.join(role.record_file());
        std::fs::remove_file(&record_path).map_err(|e| io(&record_path, &e))?;
        done.push(format!(
            "removed stale {what} lock (pid {} not running)",
            record.pid
        ));
    }
    let lock = run.join(LOCK_FILE);
    let nobody =
        read_record(root, Role::Daemon)?.is_none() && read_record(root, Role::Chat)?.is_none();
    if !done.is_empty() && nobody && lock.exists() {
        std::fs::remove_file(&lock).map_err(|e| io(&lock, &e))?;
    }
    Ok(done)
}

/// Applies a pending config migration after `config.toml.bak.<ts>`.
fn migrate(file: &ConfigFile) -> Result<Vec<String>, Error> {
    let Some(from) = pending_migration(file) else {
        return Ok(Vec::new());
    };
    let path = file.path();
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
    let backup = path.with_file_name(format!("config.toml.bak.{stamp}"));
    std::fs::copy(path, &backup).map_err(|e| io(&backup, &e))?;
    let (text, _) = file.read()?;
    file.replace(&text)?;
    Ok(vec![format!(
        "migrated config.toml from schema version {from} (backup {})",
        backup.display()
    )])
}
