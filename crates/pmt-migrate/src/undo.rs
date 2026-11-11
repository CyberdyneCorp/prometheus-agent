//! Undoing a component that did not commit (spec: hermes-migration,
//! Per-component atomicity and resumability).
//!
//! A component writes into several places (`config.toml`, the secrets
//! store, files and directories of the profile, store rows), so it cannot
//! be one transaction. Instead every write site first records how to undo
//! it here: a copy of the file or directory it is about to change, the
//! secret's previous value, or a store action. When the component fails
//! (or the ledger cannot record it), [`Undo::rollback`] replays them in
//! reverse order and the component is left absent, as if it never ran.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pmt_config::secrets::SecretStore;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_store::Store;

/// Reverts one store change.
type Revert = Box<dyn FnOnce(&Store) -> Result<(), Error>>;

enum Action {
    /// `path` held what `copy` holds now (`None`: it did not exist).
    Path {
        path: PathBuf,
        copy: Option<PathBuf>,
    },
    /// The secret `name` held `before`.
    Secret {
        store: SecretStore,
        name: String,
        before: Option<SecretString>,
    },
    Store(Revert),
}

/// What one component changed so far, and how to take it back.
pub struct Undo {
    dir: PathBuf,
    paths: BTreeSet<PathBuf>,
    secrets: BTreeSet<String>,
    actions: Vec<Action>,
}

impl Undo {
    /// A journal keeping its copies in `dir` (inside the run's staging
    /// directory, so on the profile's file system); removed when dropped.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            paths: BTreeSet::new(),
            secrets: BTreeSet::new(),
            actions: Vec::new(),
        }
    }

    /// Records the file or directory at `path` as it is now (or its
    /// absence, with the directories writing it would create); call it
    /// before changing it. Later calls for the same path keep the first
    /// state.
    ///
    /// # Errors
    ///
    /// `home.io` when the copy cannot be made: the caller must not write.
    pub fn path(&mut self, path: &Path) -> Result<(), Error> {
        let path = &created_from(path);
        if !self.paths.insert(path.clone()) {
            return Ok(());
        }
        let copy = match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io_doing(&codes::HOME_IO, "cannot read", path, &e)),
            Ok(_) => {
                let to = self.dir.join(self.actions.len().to_string());
                copy_any(path, &to)?;
                Some(to)
            }
        };
        self.actions.push(Action::Path {
            path: path.to_owned(),
            copy,
        });
        Ok(())
    }

    /// Records the secret `name` of `store` as it is now; call it before
    /// setting or removing it.
    ///
    /// # Errors
    ///
    /// The store's read error: the caller must not write.
    pub fn secret(&mut self, store: &SecretStore, name: &str) -> Result<(), Error> {
        if !self.secrets.insert(name.to_owned()) {
            return Ok(());
        }
        let before = store.get(name)?;
        self.actions.push(Action::Secret {
            store: store.clone(),
            name: name.to_owned(),
            before,
        });
        Ok(())
    }

    /// Records how to revert a store change just made.
    pub fn store(&mut self, revert: impl FnOnce(&Store) -> Result<(), Error> + 'static) {
        self.actions.push(Action::Store(Box::new(revert)));
    }

    /// Reverts everything recorded, newest first. Every action is tried
    /// even when one fails.
    ///
    /// # Errors
    ///
    /// `migrate.component` listing what could not be reverted.
    pub fn rollback(mut self, store: &Store) -> Result<(), Error> {
        let mut failures = Vec::new();
        while let Some(action) = self.actions.pop() {
            if let Err(e) = revert(action, store) {
                failures.push(e.message().to_owned());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::new(&codes::MIGRATE_COMPONENT, failures.join("; ")))
        }
    }
}

impl Drop for Undo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn revert(action: Action, store: &Store) -> Result<(), Error> {
    match action {
        Action::Path { path, copy } => {
            remove_any(&path)?;
            match copy {
                Some(copy) => copy_any(&copy, &path),
                None => Ok(()),
            }
        }
        Action::Secret {
            store: secrets,
            name,
            before,
        } => match before {
            Some(value) => secrets.set(&name, &value),
            None => secrets.remove(&name).map(drop),
        },
        Action::Store(revert) => revert(store),
    }
}

/// The outermost missing directory on the way to a missing `path` (or
/// `path` itself): writing `path` creates it, so undoing removes it.
fn created_from(path: &Path) -> PathBuf {
    let mut top = path;
    while let Some(parent) = top.parent()
        && !parent.as_os_str().is_empty()
        && std::fs::symlink_metadata(parent).is_err()
    {
        top = parent;
    }
    top.to_owned()
}

/// Removes the file or directory at `path` (nothing when absent).
fn remove_any(path: &Path) -> Result<(), Error> {
    let removed = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
    removed.map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot restore", path, &e))
}

/// Copies the file, symbolic link or directory tree `from` to `to`
/// (permissions kept).
fn copy_any(from: &Path, to: &Path) -> Result<(), Error> {
    let io =
        |path: &Path, e: std::io::Error| Error::io_doing(&codes::HOME_IO, "cannot copy", path, &e);
    let meta = std::fs::symlink_metadata(from).map_err(|e| io(from, e))?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    if meta.is_symlink() {
        return copy_link(from, to).map_err(|e| io(from, e));
    }
    if !meta.is_dir() {
        return std::fs::copy(from, to).map(drop).map_err(|e| io(from, e));
    }
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| io(from, e))? {
        let entry = entry.map_err(|e| io(from, e))?;
        copy_any(&entry.path(), &to.join(entry.file_name()))?;
    }
    std::fs::set_permissions(to, meta.permissions()).map_err(|e| io(to, e))
}

#[cfg(unix)]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_and_directories_return_to_their_state_before_the_component() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = crate::testkit::target(root);
        std::fs::write(root.join("kept.toml"), "a = 1\n").unwrap();
        std::fs::create_dir_all(root.join("skills/old")).unwrap();
        std::fs::write(root.join("skills/old/SKILL.md"), "old\n").unwrap();
        let mut undo = Undo::new(root.join("undo"));
        undo.path(&root.join("kept.toml")).unwrap();
        undo.path(&root.join("new.md")).unwrap();
        undo.path(&root.join("skills/new/category/SKILL.md"))
            .unwrap();
        undo.path(&root.join("skills/old")).unwrap();
        std::fs::write(root.join("kept.toml"), "a = 2\n").unwrap();
        std::fs::write(root.join("new.md"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("skills/new/category")).unwrap();
        std::fs::write(root.join("skills/new/category/SKILL.md"), "x\n").unwrap();
        std::fs::remove_dir_all(root.join("skills/old")).unwrap();
        std::fs::create_dir_all(root.join("skills/old")).unwrap();
        std::fs::write(root.join("skills/old/SKILL.md"), "replaced\n").unwrap();
        // A second record of the same path keeps the first state.
        undo.path(&root.join("kept.toml")).unwrap();
        undo.rollback(&target.store).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("kept.toml")).unwrap(),
            "a = 1\n"
        );
        assert!(!root.join("new.md").exists());
        assert!(!root.join("skills/new").exists(), "created parents go too");
        assert_eq!(
            std::fs::read_to_string(root.join("skills/old/SKILL.md")).unwrap(),
            "old\n"
        );
        assert!(!root.join("undo").exists(), "the copies are removed");
    }
}
