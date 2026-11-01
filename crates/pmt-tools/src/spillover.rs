//! Spillover files of oversized tool results and their retention (spec:
//! tool-system, Result budget and spillover; Spillover retention).
//!
//! Files live at `<profile>/cache/spillover/<session_id>/<call_id>.txt`
//! (directories 0700, files 0600). They are deleted once older than
//! `sessions.retention_secs`, or 24 hours when that is 0, and a session's
//! directory goes with the session (the store removes it on deletion).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pmt_config::layout::spillover_root;
use pmt_config::schema::SessionsConfig;

/// Retention when `sessions.retention_secs` is 0.
pub const DEFAULT_RETENTION: Duration = Duration::from_hours(24);

/// `<profile>/cache/spillover/<session_id>`.
#[must_use]
pub fn session_dir(profile_dir: &Path, session_id: &str) -> PathBuf {
    spillover_root(profile_dir).join(session_id)
}

/// How long spillover files are kept.
#[must_use]
pub fn retention(sessions: &SessionsConfig) -> Duration {
    match sessions.retention_secs {
        0 => DEFAULT_RETENTION,
        secs => Duration::from_secs(secs),
    }
}

/// Removes files under `root/<session>/` last modified more than
/// `retention` before `now`, then the session directories left empty.
/// Returns how many files were removed.
#[allow(clippy::must_use_candidate)] // pruning is the point; the count is informational
pub fn prune(root: &Path, now: SystemTime, retention: Duration) -> usize {
    let Ok(sessions) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for session in sessions.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        for file in fs::read_dir(&session).into_iter().flatten().flatten() {
            let old = file
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age > retention)
                });
            if old && fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
        // Only succeeds when empty.
        let _ = fs::remove_dir(&session);
    }
    removed
}

/// Prunes the profile's spillover per `sessions` (run at session start).
#[allow(clippy::must_use_candidate)]
pub fn prune_profile(profile_dir: &Path, sessions: &SessionsConfig) -> usize {
    prune(
        &spillover_root(profile_dir),
        SystemTime::now(),
        retention(sessions),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use pmt_config::schema::SessionsConfig;

    use super::{DEFAULT_RETENTION, prune, retention, session_dir};

    #[test]
    fn retention_follows_sessions_config() {
        let sessions = |retention_secs| SessionsConfig {
            retention_secs,
            ..SessionsConfig::default()
        };
        assert_eq!(retention(&sessions(0)), DEFAULT_RETENTION);
        assert_eq!(retention(&sessions(3_600)), Duration::from_hours(1));
    }

    #[test]
    fn old_files_and_empty_session_dirs_are_pruned() {
        let profile = tempfile::tempdir().unwrap();
        let (old, fresh) = (
            session_dir(profile.path(), "ses_old"),
            session_dir(profile.path(), "ses_new"),
        );
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(old.join("call_1.txt"), "x").unwrap();
        fs::write(fresh.join("call_2.txt"), "y").unwrap();
        let root = old.parent().unwrap().to_owned();
        assert!(root.ends_with("cache/spillover"));
        // Seen from 25 hours later every file is older than the 24 h default.
        let later = SystemTime::now() + Duration::from_hours(25);
        assert_eq!(prune(&root, SystemTime::now(), DEFAULT_RETENTION), 0);
        assert_eq!(prune(&root, later, DEFAULT_RETENTION), 2);
        assert!(!old.exists() && !fresh.exists());
    }
}
