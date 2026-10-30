//! Which files of a home a backup or snapshot holds (spec: configuration-
//! and-profiles, Whole-home backup, State snapshots).
//!
//! A member is a regular file named by its `/`-separated path relative to
//! the home. Directories, symlinks, sockets and other special files are
//! never members. The exclusion rules apply on both sides: a backup never
//! writes an excluded member and a restore never restores one, even if an
//! archive carries it.

use std::path::{Component, Path, PathBuf};

/// Home-level directories never archived: managed runtimes (re-
/// installable), runtime records and sockets, logs, caches and earlier
/// backups.
const EXCLUDED_HOME_DIRS: &[&str] = &["runtimes", "run", "logs", "cache", "backups"];
/// Profile-level directories never archived: logs, caches (browser
/// profiles live under `cache/browser/`) and earlier config/state backups.
const EXCLUDED_PROFILE_DIRS: &[&str] = &["logs", "cache", "backups"];

/// File-name suffixes never archived: lock files and SQLite sidecars (a
/// database is copied whole through the online backup API instead).
const EXCLUDED_SUFFIXES: &[&str] = &[".lock", ".db-wal", ".db-shm", ".db-journal"];

/// Files of the home root kept by every backup, `--quick` included.
pub const HOME_FILES: &[&str] = &["active_profile", "daemon.toml"];

/// The critical per-profile state kept by `--quick` and by snapshots:
/// files, then directories (cron job definitions, pairing and allowlist
/// data). Groups that add critical state (the projects store, pairing)
/// keep its path here.
pub const QUICK_PROFILE_FILES: &[&str] = &[
    "config.toml",
    "secrets.toml",
    "grants.toml",
    "state.db",
    "projects.toml",
];
/// Directories of [`QUICK_PROFILE_FILES`]' critical state.
pub const QUICK_PROFILE_DIRS: &[&str] = &["cron", "pairing"];

/// Whether `relative` (a home-relative member path) may be backed up or
/// restored.
#[must_use]
pub fn allowed(relative: &str) -> bool {
    let parts: Vec<&str> = relative.split('/').collect();
    let (dirs, file) = parts.split_at(parts.len() - 1);
    let file = file[0];
    !excluded_dir(dirs)
        && !EXCLUDED_SUFFIXES.iter().any(|s| file.ends_with(s))
        && !file.contains(".tmp.")
}

/// Whether a directory (as its path components) or any of its parents is
/// excluded.
fn excluded_dir(dirs: &[&str]) -> bool {
    let home_level = dirs.first().is_some_and(|d| EXCLUDED_HOME_DIRS.contains(d));
    let profile_level = dirs.first() == Some(&"profiles")
        && dirs
            .get(2)
            .is_some_and(|d| EXCLUDED_PROFILE_DIRS.contains(d));
    home_level || profile_level
}

/// Whether `relative` belongs to the `--quick` set.
#[must_use]
pub fn quick(relative: &str) -> bool {
    if HOME_FILES.contains(&relative) {
        return true;
    }
    let Some(rest) = relative.strip_prefix("profiles/") else {
        return false;
    };
    let Some((_profile, inside)) = rest.split_once('/') else {
        return false;
    };
    profile_quick(inside)
}

/// Whether a path relative to a profile directory is critical state.
#[must_use]
pub fn profile_quick(inside: &str) -> bool {
    QUICK_PROFILE_FILES.contains(&inside)
        || QUICK_PROFILE_DIRS
            .iter()
            .any(|dir| inside.strip_prefix(dir).is_some_and(|r| r.starts_with('/')))
}

/// The profile a member belongs to (`profiles/<name>/...`).
#[must_use]
pub fn profile_of(relative: &str) -> Option<&str> {
    let rest = relative.strip_prefix("profiles/")?;
    let (name, _) = rest.split_once('/')?;
    Some(name)
}

/// Whether a member is a SQLite database (copied with the backup API).
#[must_use]
pub fn is_database(relative: &str) -> bool {
    Path::new(relative)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
}

/// Checks that an archived path is a plain relative path (no `..`, root
/// or prefix) and returns it with `/` separators.
#[must_use]
pub fn safe_relative(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The on-disk path of a member.
#[must_use]
pub fn on_disk(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_owned(), |path, part| path.join(part))
}

/// Every member under `root` (relative to `root`, sorted) accepted by
/// `keep`, and the paths that could not be read.
#[must_use]
pub fn walk(root: &Path, keep: &dyn Fn(&str) -> bool) -> (Vec<String>, Vec<String>) {
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    let mut stack = vec![(root.to_owned(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            unreadable.push(prefix.trim_end_matches('/').to_owned());
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = format!("{prefix}{name}");
            match entry.file_type() {
                Ok(kind)
                    if kind.is_dir() && !excluded_dir(&relative.split('/').collect::<Vec<_>>()) =>
                {
                    stack.push((entry.path(), format!("{relative}/")));
                }
                Ok(kind) if kind.is_file() && allowed(&relative) && keep(&relative) => {
                    found.push(relative);
                }
                _ => {}
            }
        }
    }
    found.sort();
    unreadable.sort();
    (found, unreadable)
}

#[cfg(test)]
mod tests {
    use super::{allowed, profile_of, quick, safe_relative};

    #[test]
    fn exclusions() {
        for excluded in [
            "runtimes/python/bin/python",
            "run/gateway.json",
            "logs/daemon.log",
            "profiles/a/cache/browser/real-profile/Cookies",
            "profiles/a/logs/x.log",
            "backups/prometheus-1.tar.zst",
            "profiles/a/backups/config/config-1.toml",
            "profiles/a/state.db-wal",
            "profiles/a/state.db-shm",
            "run/gateway.lock",
            "profiles/a/secrets.toml.lock",
            "profiles/a/config.toml.tmp.42",
        ] {
            assert!(!allowed(excluded), "{excluded}");
        }
        for kept in [
            "active_profile",
            "profiles/a/state.db",
            "profiles/a/skills/x/SKILL.md",
            "profiles/a/skills/run/logs/SKILL.md",
        ] {
            assert!(allowed(kept), "{kept}");
        }
    }

    #[test]
    fn quick_set() {
        assert!(quick("daemon.toml"));
        assert!(quick("profiles/a/state.db"));
        assert!(quick("profiles/a/cron/jobs.toml"));
        assert!(!quick("profiles/a/skills/x.md"));
        assert!(!quick("profiles/a/cronx"));
        assert_eq!(profile_of("profiles/a/state.db"), Some("a"));
        assert_eq!(profile_of("active_profile"), None);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        use std::path::Path;
        assert_eq!(
            safe_relative(Path::new("profiles/a/x")).as_deref(),
            Some("profiles/a/x")
        );
        assert!(safe_relative(Path::new("../x")).is_none());
        assert!(safe_relative(Path::new("/etc/passwd")).is_none());
        assert!(safe_relative(Path::new("a/../../x")).is_none());
    }
}
