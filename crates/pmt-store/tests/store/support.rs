//! A temporary home with profiles, and raw access to a store file for
//! simulating other writers.

use std::path::{Path, PathBuf};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{NewSession, Session, Source, Store};

pub(crate) struct TestHome {
    // Keeps a temporary home alive.
    _root: Option<tempfile::TempDir>,
    pub home: Home,
}

impl TestHome {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        Self {
            _root: Some(root),
            home,
        }
    }

    pub(crate) fn at(root: &Path) -> Self {
        Self {
            _root: None,
            home: Home::open(root).unwrap(),
        }
    }

    pub(crate) fn scope(&self, name: &str) -> ProfileScope {
        self.scope_with(name, &[])
    }

    pub(crate) fn scope_with(&self, name: &str, overrides: &[&str]) -> ProfileScope {
        let profile = self
            .home
            .profile(&ProfileName::parse(name).unwrap())
            .unwrap();
        ProfileScope::open(
            profile,
            &Overrides::parse(overrides).unwrap(),
            Launch::Other,
        )
        .unwrap()
    }

    pub(crate) fn store(&self, name: &str) -> Store {
        Store::open(&self.scope(name)).unwrap()
    }

    pub(crate) fn database(&self, name: &str) -> PathBuf {
        self.scope(name).path("state.db")
    }

    /// A copy of this home at a new path: opening its stores is a first
    /// open in this process (as after a restart).
    pub(crate) fn copied(&self) -> Self {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("home");
        copy_dir(self.home.root(), &target);
        Self {
            _root: Some(root),
            home: Home::open(&target).unwrap(),
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub(crate) fn session(store: &Store) -> Session {
    store.create_session(&NewSession::new(Source::Cli)).unwrap()
}

/// A second connection, as another process or a manual edit would have.
pub(crate) fn raw(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn
}
