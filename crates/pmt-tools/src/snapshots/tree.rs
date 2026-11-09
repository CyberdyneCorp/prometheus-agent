//! Where a session's files live, as snapshots see them (spec:
//! execution-environments, File snapshots and undo; Snapshot triggers,
//! caps and per-run toggle): this host ([`HostTree`]), a container whose
//! workspace is a bind mount of a host directory ([`MountedTree`]: its
//! paths located on the host and handled there), or any other backend's
//! own filesystem ([`BackendTree`]: every read, write and listing goes
//! through its [`FileBackend`]).
//!
//! A session's path (what its file tools and terminal name) becomes a tree
//! path through [`Tree::locate`]; journals and checkpoints hold tree paths.
//! [`Tree::key`] names a project in the snapshot store, so `/srv/app` of a
//! remote host and of this one are never mixed up.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::files::{FileBackend, Kind};

/// A filesystem snapshots read and restore.
pub trait Tree: Send + Sync + std::fmt::Debug {
    /// The content of `path`.
    ///
    /// # Errors
    ///
    /// It cannot be read (`NotFound` when missing).
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// The size of the file `path`.
    ///
    /// # Errors
    ///
    /// It cannot be read.
    fn size(&self, path: &Path) -> io::Result<u64>;

    /// Replaces `path` with `bytes`, creating missing parents.
    ///
    /// # Errors
    ///
    /// It cannot be written.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Removes the file `path`; a missing file is not an error.
    ///
    /// # Errors
    ///
    /// It cannot be removed.
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Whether `path` is a directory.
    fn is_dir(&self, path: &Path) -> bool;

    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;

    /// The files of the project at `root` now (its `.gitignore` files
    /// respected, `.git`, nested repositories and `excluded` left out):
    /// relative path (`/`-separated) to path, and whether `max` cut the
    /// walk short.
    fn walk(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        max: usize,
    ) -> (BTreeMap<String, PathBuf>, bool);

    /// The project `root`'s name in the snapshot store.
    fn key(&self, root: &Path) -> PathBuf;

    /// Where the session's `path` (as its file tools and terminal name
    /// it) is in this tree; `None` when this host cannot reach it.
    fn locate(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    /// The backend whose own filesystem this is (`None`: this host's);
    /// its journaled edits can be undone only through that backend.
    fn backend(&self) -> Option<&str> {
        None
    }
}

/// This host's filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostTree;

impl Tree for HostTree {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        fs::metadata(path).map(|m| m.len())
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn walk(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        max: usize,
    ) -> (BTreeMap<String, PathBuf>, bool) {
        host_walk(root, excluded, max)
    }

    fn key(&self, root: &Path) -> PathBuf {
        root.to_path_buf()
    }
}

fn host_walk(root: &Path, excluded: &[PathBuf], max: usize) -> (BTreeMap<String, PathBuf>, bool) {
    let top = root.to_path_buf();
    let excluded = excluded.to_vec();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let path = entry.path();
            if entry.file_name() == ".git" || excluded.iter().any(|e| path.starts_with(e)) {
                return false;
            }
            // A nested repository is a project of its own.
            !(path != top
                && entry.file_type().is_some_and(|t| t.is_dir())
                && path.join(".git").exists())
        })
        .build();
    let mut files = BTreeMap::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if files.len() >= max {
            return (files, true);
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            files.insert(rel, entry.path().to_path_buf());
        }
    }
    (files, false)
}

/// A container whose `inside` directory (its `/workspace`) is a bind
/// mount of the host directory `host`: the session's paths below the
/// mount are [`Tree::locate`]d on the host, where they are read, recorded
/// and restored like any host file (the container sees the same files).
/// Paths outside the mount are not reachable from this host.
#[derive(Debug, Clone)]
pub struct MountedTree {
    inside: PathBuf,
    host: PathBuf,
}

impl MountedTree {
    /// The mount of host directory `host` at `inside`.
    #[must_use]
    pub fn new(inside: &Path, host: &Path) -> Self {
        Self {
            inside: inside.to_path_buf(),
            host: host.to_path_buf(),
        }
    }
}

impl Tree for MountedTree {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        HostTree.read(path)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        HostTree.size(path)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        HostTree.write(path, bytes)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        HostTree.remove(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn walk(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        max: usize,
    ) -> (BTreeMap<String, PathBuf>, bool) {
        host_walk(root, excluded, max)
    }

    fn key(&self, root: &Path) -> PathBuf {
        root.to_path_buf()
    }

    fn locate(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.inside)
            .ok()
            .map(|rel| self.host.join(rel))
    }
}

/// Deepest directory level a [`BackendTree`] walk descends to (links are
/// followed by backends, so a cycle must end somewhere).
const MAX_DEPTH: usize = 64;

/// A backend's own filesystem (SSH host, plugin sandbox, a container
/// without a workspace mount), reached through its [`FileBackend`]. A
/// walk costs a backend call per directory and a read per file.
pub struct BackendTree {
    files: Arc<dyn FileBackend>,
    /// Prefix of [`Tree::key`]: the backend's name.
    name: String,
}

impl std::fmt::Debug for BackendTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendTree")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl BackendTree {
    /// The tree of `files`, named `name` (the terminal backend).
    #[must_use]
    pub fn new(name: &str, files: Arc<dyn FileBackend>) -> Self {
        Self {
            files,
            name: name.to_owned(),
        }
    }

    /// The `.gitignore` of `dir`, when it has one.
    fn ignore_of(&self, dir: &Path) -> Option<ignore::gitignore::Gitignore> {
        let text = self.files.read(&dir.join(".gitignore")).ok()?;
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        for line in String::from_utf8_lossy(&text).lines() {
            let _ = builder.add_line(None, line);
        }
        builder.build().ok()
    }
}

/// One directory of a [`BackendTree`] walk with the ignore rules above it.
struct Pending {
    dir: PathBuf,
    depth: usize,
    rules: Vec<Arc<ignore::gitignore::Gitignore>>,
}

/// A [`BackendTree`] walk in progress.
struct Walk<'a> {
    tree: &'a BackendTree,
    root: &'a Path,
    excluded: &'a [PathBuf],
    max: usize,
    files: BTreeMap<String, PathBuf>,
    stack: Vec<Pending>,
}

impl Walk<'_> {
    /// Lists one directory: its files are kept, its subdirectories queued.
    /// Returns whether the file cap cut the walk short.
    fn visit(&mut self, pending: Pending) -> bool {
        let Pending {
            dir,
            depth,
            mut rules,
        } = pending;
        let Ok(entries) = self.tree.files.list_dir(&dir) else {
            return false;
        };
        if let Some(own) = self.tree.ignore_of(&dir) {
            rules.push(Arc::new(own));
        }
        for entry in entries {
            let path = dir.join(&entry.name);
            let is_dir = entry.kind == Kind::Dir;
            if self.ignored(&entry.name, &path, is_dir, &rules) {
                continue;
            }
            if is_dir {
                // A nested repository is a project of its own.
                if depth < MAX_DEPTH && !self.tree.exists(&path.join(".git")) {
                    self.stack.push(Pending {
                        dir: path,
                        depth: depth + 1,
                        rules: rules.clone(),
                    });
                }
            } else if entry.kind == Kind::File && self.keep(path) {
                return true;
            }
        }
        false
    }

    fn ignored(
        &self,
        name: &str,
        path: &Path,
        is_dir: bool,
        rules: &[Arc<ignore::gitignore::Gitignore>],
    ) -> bool {
        name == ".git"
            || self.excluded.iter().any(|e| path.starts_with(e))
            || rules.iter().any(|r| r.matched(path, is_dir).is_ignore())
    }

    /// Keeps a file; returns whether the cap was already reached.
    fn keep(&mut self, path: PathBuf) -> bool {
        if self.files.len() >= self.max {
            return true;
        }
        if let Ok(rel) = path.strip_prefix(self.root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            self.files.insert(rel, path);
        }
        false
    }
}

impl Tree for BackendTree {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files.read(path)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        self.files.metadata(path).map(|m| m.len)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.files.write(path, bytes)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match self.files.remove(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.files.metadata(path).is_ok_and(|m| m.kind == Kind::Dir)
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.metadata(path).is_ok()
    }

    fn walk(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        max: usize,
    ) -> (BTreeMap<String, PathBuf>, bool) {
        let mut walk = Walk {
            tree: self,
            root,
            excluded,
            max,
            files: BTreeMap::new(),
            stack: vec![Pending {
                dir: root.to_path_buf(),
                depth: 0,
                rules: Vec::new(),
            }],
        };
        while let Some(pending) = walk.stack.pop() {
            if walk.visit(pending) {
                return (walk.files, true);
            }
        }
        (walk.files, false)
    }

    fn key(&self, root: &Path) -> PathBuf {
        PathBuf::from(format!("{}:{}", self.name, root.display()))
    }

    fn backend(&self) -> Option<&str> {
        Some(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mounted_tree_locates_paths_below_the_mount_on_the_host() {
        let tree = MountedTree::new(Path::new("/workspace"), Path::new("/home/u/project"));
        assert_eq!(
            tree.locate(Path::new("/workspace/src/a.rs")),
            Some(PathBuf::from("/home/u/project/src/a.rs"))
        );
        assert_eq!(
            tree.locate(Path::new("/workspace")),
            Some(PathBuf::from("/home/u/project"))
        );
        assert_eq!(tree.locate(Path::new("/etc/passwd")), None);
        assert_eq!(tree.backend(), None);
    }
}
