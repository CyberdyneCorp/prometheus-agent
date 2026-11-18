//! Per-checkout install state and its retention (spec: git-worktrees,
//! Shared install state retention).
//!
//! State a tool keeps for one checkout outside the checkout itself
//! (language-server caches keyed by workspace root, package stores keyed
//! by checkout) outlives the tree when the tree is removed. Every such
//! directory carries a [`RECORD`] naming its checkout and when it was last
//! used; whoever uses the state calls [`record`]. The sweep removes a
//! directory once its checkout no longer exists and it has been unused
//! for `worktrees.install_state_retention_secs` (0 keeps it forever). A
//! checkout that still exists keeps its state however old: the state is
//! rebuilt on demand anyway, so only state nothing can use again goes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sweep::Mode;

/// The record file inside a state directory.
pub const RECORD: &str = ".prometheus-checkout.json";

/// How deep under `<profile>/cache` state directories are looked for
/// (`cache/lsp/<server>/<key>` is three levels).
const MAX_DEPTH: usize = 3;

/// What a state directory's record says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The checkout the state belongs to.
    pub checkout: PathBuf,
    /// When the state was last used (Unix seconds).
    pub last_used: u64,
}

/// Marks `state_dir` as the install state of `checkout`, used at `now`
/// (Unix seconds). Advisory: a failure is logged, never fatal to the
/// tool keeping the state.
pub fn record(state_dir: &Path, checkout: &Path, now: u64) {
    let record = Record {
        checkout: checkout.to_path_buf(),
        last_used: now,
    };
    let written = pmt_core::private_fs::create_dir(state_dir).and_then(|()| {
        let text = serde_json::to_vec(&record).unwrap_or_default();
        pmt_core::private_fs::write_atomic(&state_dir.join(RECORD), &text)
    });
    if let Err(err) = written {
        tracing::debug!(dir = %state_dir.display(), error = %err, "install state record not written");
    }
}

/// One state directory the sweep decided on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reclaimed {
    /// The state directory.
    pub path: PathBuf,
    /// The checkout it belonged to.
    pub checkout: PathBuf,
    /// Seconds since it was last used.
    pub idle_secs: u64,
    /// It was removed (false on a dry run or when removal failed).
    pub removed: bool,
}

/// The state directories under `<profile>/cache`, with their records.
#[must_use]
pub fn states(profile: &Path) -> Vec<(PathBuf, Record)> {
    let mut found = Vec::new();
    collect(&profile.join("cache"), MAX_DEPTH, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn collect(dir: &Path, depth: usize, found: &mut Vec<(PathBuf, Record)>) {
    if let Some(record) = read_record(dir) {
        found.push((dir.to_path_buf(), record));
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        // Never follow links out of the cache.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect(&entry.path(), depth - 1, found);
        }
    }
}

fn read_record(dir: &Path) -> Option<Record> {
    let text = std::fs::read(dir.join(RECORD)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Whether the sweep removes a state at `now`.
#[must_use]
pub fn expired(record: &Record, retention_secs: u64, now: u64) -> bool {
    retention_secs != 0
        && !record.checkout.exists()
        && now.saturating_sub(record.last_used) > retention_secs
}

/// Removes (on [`Mode::Apply`]) the install state under `profile` whose
/// checkout is gone and which has been unused for `retention_secs`.
#[must_use]
pub fn sweep(profile: &Path, retention_secs: u64, now: u64, mode: Mode) -> Vec<Reclaimed> {
    states(profile)
        .into_iter()
        .filter(|(_, record)| expired(record, retention_secs, now))
        .map(|(path, record)| {
            let removed = mode == Mode::Apply && remove(&path);
            Reclaimed {
                idle_secs: now.saturating_sub(record.last_used),
                checkout: record.checkout,
                path,
                removed,
            }
        })
        .collect()
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(dir = %path.display(), error = %err, "install state not removed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    const RETENTION: u64 = 14 * DAY;

    fn state(profile: &Path, key: &str, checkout: &Path, last_used: u64) -> PathBuf {
        let dir = profile.join("cache/lsp/rust-analyzer").join(key);
        record(&dir, checkout, last_used);
        dir
    }

    #[test]
    fn state_of_a_checkout_deleted_20_days_ago_is_removed() {
        let profile = tempfile::tempdir().unwrap();
        let live = profile.path().join("live-checkout");
        std::fs::create_dir_all(&live).unwrap();
        let gone = profile.path().join("deleted-checkout");
        let now = 100 * DAY;
        let old = state(profile.path(), "a1", &gone, now - 20 * DAY);
        let recent = state(profile.path(), "b2", &gone, now - 3 * DAY);
        let kept = state(profile.path(), "c3", &live, now - 60 * DAY);

        let planned = sweep(profile.path(), RETENTION, now, Mode::DryRun);
        assert_eq!(planned.len(), 1);
        assert!(!planned[0].removed && old.exists());

        let swept = sweep(profile.path(), RETENTION, now, Mode::Apply);
        assert_eq!(swept[0].path, old);
        assert_eq!(swept[0].idle_secs, 20 * DAY);
        assert!(swept[0].removed);
        assert!(!old.exists());
        assert!(recent.exists() && kept.exists());
    }

    #[test]
    fn a_zero_retention_keeps_state_forever() {
        let profile = tempfile::tempdir().unwrap();
        let gone = profile.path().join("deleted-checkout");
        let dir = state(profile.path(), "a1", &gone, 0);
        assert!(sweep(profile.path(), 0, 400 * DAY, Mode::Apply).is_empty());
        assert!(dir.exists());
    }
}
