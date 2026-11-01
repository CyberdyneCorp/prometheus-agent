//! Where a profile's store lives (design D13: `<profile>/state.db` and
//! `<profile>/blobs/`). A [`StoreLocation`] is obtainable only from a
//! loaded [`ProfileScope`], so store code cannot reach a profile's data
//! from a bare path or from ambient state (spec: configuration-and-profiles,
//! Explicit `ProfileScope`). Opening the database (group 6) starts here.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;

/// The SQLite database file inside a profile directory.
pub const DATABASE_FILE: &str = "state.db";

/// The content-addressed blob directory inside a profile directory.
pub const BLOBS_DIR: &str = "blobs";

/// Where a corrupt `state.db` is moved, inside a profile directory.
pub const QUARANTINE_DIR: &str = "quarantine";

/// The store files of one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLocation {
    database: PathBuf,
    blobs: PathBuf,
    quarantine: PathBuf,
    spillover: PathBuf,
}

impl StoreLocation {
    /// The store of the profile `scope` was loaded for.
    #[must_use]
    pub fn of(scope: &ProfileScope) -> Self {
        Self {
            database: scope.path(DATABASE_FILE),
            blobs: scope.path(BLOBS_DIR),
            quarantine: scope.path(QUARANTINE_DIR),
            spillover: pmt_config::layout::spillover_root(scope.dir()),
        }
    }

    /// `<profile>/state.db`.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.database
    }

    /// `<profile>/blobs/`.
    #[must_use]
    pub fn blobs(&self) -> &Path {
        &self.blobs
    }

    /// `<profile>/quarantine/`: corrupt databases and their salvage
    /// reports.
    #[must_use]
    pub fn quarantine(&self) -> &Path {
        &self.quarantine
    }

    /// `<profile>/cache/spillover/`: oversized tool results, one directory
    /// per session, removed with the session.
    #[must_use]
    pub fn spillover(&self) -> &Path {
        &self.spillover
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    use super::StoreLocation;

    #[test]
    fn each_profile_has_its_own_store() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let open = |name: &str| {
            let profile = home.profile(&ProfileName::parse(name).unwrap()).unwrap();
            ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
        };
        let (default, work) = (open("default"), open("work"));
        let (a, b) = (StoreLocation::of(&default), StoreLocation::of(&work));
        assert_eq!(a.database(), default.dir().join("state.db"));
        assert_eq!(a.blobs(), default.dir().join("blobs"));
        assert_ne!(a.database(), b.database());
        assert!(b.database().starts_with(work.dir()));
    }
}
