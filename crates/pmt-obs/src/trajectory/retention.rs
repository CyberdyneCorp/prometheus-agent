//! The trajectory retention sweep (spec: observability, Trajectory
//! retention and size limits): part of the daily maintenance, it deletes
//! the trajectories of unpinned sessions that ended more than
//! `observability.trajectories.retention_secs` ago (0 keeps them forever)
//! and records `trajectory_purged_at` on the session. Trajectories whose
//! session no longer exists (deleted or pruned) go too, once the file is a
//! day old, so a deletion never leaves a transcript copy behind.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::scope::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;

use super::files;

/// Age after which a trajectory without a session row is removed (a new
/// session's file can briefly exist before its row is visible).
pub const ORPHAN_GRACE: Duration = Duration::from_hours(24);

/// What one sweep removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    /// Sessions whose trajectory was purged (`trajectory_purged_at` set).
    pub purged: Vec<SessionId>,
    /// Trajectory files of sessions that no longer exist.
    pub orphans: Vec<PathBuf>,
}

/// Runs the store's daily maintenance and, when it ran, the trajectory
/// sweep of the scope's profile. Returns `None` when maintenance already
/// ran within the last day.
///
/// # Errors
///
/// Store and file errors.
pub fn daily(store: &Store, scope: &ProfileScope) -> Result<Option<Swept>, Error> {
    let config = scope.config();
    if !store.maintenance(&config.sessions)?.ran {
        return Ok(None);
    }
    let retention = config.observability.trajectories.retention_secs;
    sweep(store, &scope.path(files::DIR), retention, Timestamp::now()).map(Some)
}

/// Deletes the trajectories in `dir` that retention (`retention_secs`, 0 =
/// forever) or session deletion made due as of `now`.
///
/// # Errors
///
/// Store errors; `trajectory.io` when a file cannot be removed.
pub fn sweep(
    store: &Store,
    dir: &Path,
    retention_secs: u64,
    now: Timestamp,
) -> Result<Swept, Error> {
    let mut swept = Swept::default();
    if retention_secs > 0 {
        let cutoff = now - Duration::from_secs(retention_secs);
        for id in store.trajectory_purge_candidates(cutoff)? {
            files::remove(dir, &id.to_string()).map_err(|err| io_error(dir, &err))?;
            store.mark_trajectory_purged(id)?;
            swept.purged.push(id);
        }
    }
    swept.orphans = remove_orphans(store, dir, now)?;
    Ok(swept)
}

fn remove_orphans(store: &Store, dir: &Path, now: Timestamp) -> Result<Vec<PathBuf>, Error> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = files::session_of(&path).and_then(|s| SessionId::parse(s).ok()) else {
            continue;
        };
        if !older_than(&path, now, ORPHAN_GRACE) || session_exists(store, id)? {
            continue;
        }
        std::fs::remove_file(&path).map_err(|err| io_error(&path, &err))?;
        removed.push(path);
    }
    Ok(removed)
}

fn session_exists(store: &Store, id: SessionId) -> Result<bool, Error> {
    match store.session(id) {
        Ok(_) => Ok(true),
        Err(err) if err.code() == &codes::SESSION_NOT_FOUND => Ok(false),
        Err(err) => Err(err),
    }
}

fn older_than(path: &Path, now: Timestamp, age: Duration) -> bool {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| Timestamp::try_from(time).ok());
    modified.is_some_and(|modified| modified + age < now)
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::new(
        &codes::TRAJECTORY_IO,
        format!("cannot remove {}: {err}", path.display()),
    )
}
