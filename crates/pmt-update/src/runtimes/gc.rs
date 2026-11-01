//! `prometheus runtimes gc` (spec: distribution-and-update, Runtime leases
//! and garbage collection): removes the versions that neither the installed
//! binary nor its `.prev` pins and that no process leases, plus partial
//! downloads and staging directories older than 24 hours.

use pmt_core::error::codes;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pmt_core::error::Error;
use serde::Serialize;

use super::{Store, dir_size, lease, remove_dir, version_dirs};

/// Partial downloads and staging directories younger than this belong to
/// an install that may still be running.
pub const PARTIAL_GRACE: Duration = Duration::from_hours(24);

/// What `gc` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GcReport {
    /// Removed version directories and stale partial files.
    pub removed: Vec<PathBuf>,
    /// Unpinned versions kept because a process leases them.
    pub leased: Vec<PathBuf>,
    /// Bytes reclaimed.
    pub reclaimed: u64,
}

/// Collects garbage in `store`. `pinned` holds, by runtime name, the
/// version directories that must stay (this binary's pins plus those of
/// the other installed binary: see [`Store::pins`]).
///
/// # Errors
///
/// `runtime.io` when a directory cannot be removed.
pub fn collect(
    store: &Store,
    pinned: &BTreeMap<String, BTreeSet<String>>,
    now: SystemTime,
) -> Result<GcReport, Error> {
    let mut report = GcReport::default();
    for name in store
        .index()
        .runtimes
        .keys()
        .chain(stray_names(store).iter())
    {
        let keep = pinned.get(name);
        for dir in version_dirs(&store.root().join(name)) {
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if keep.is_some_and(|k| k.contains(&dir_name)) {
                continue;
            }
            if lease::users(store.root(), name, &dir_name) > 0 {
                report.leased.push(dir);
                continue;
            }
            report.reclaimed += dir_size(&dir);
            remove_dir(&dir)?;
            lease::forget(store.root(), name, &dir_name);
            report.removed.push(dir);
        }
    }
    for dir in [store.downloads(), store.staging()] {
        sweep_old(&dir, now, &mut report)?;
    }
    Ok(report)
}

/// Runtime directories of names this binary no longer pins (a runtime
/// dropped from the index): all their versions are unpinned.
fn stray_names(store: &Store) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(store.root()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && !store.index().runtimes.contains_key(n))
        .collect()
}

fn sweep_old(dir: &Path, now: SystemTime, report: &mut GcReport) -> Result<(), Error> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age < PARTIAL_GRACE {
            continue;
        }
        if meta.is_dir() {
            report.reclaimed += dir_size(&path);
            remove_dir(&path)?;
        } else {
            report.reclaimed += meta.len();
            std::fs::remove_file(&path).map_err(|e| Error::io(&codes::RUNTIME_IO, &path, &e))?;
        }
        report.removed.push(path);
    }
    Ok(())
}
