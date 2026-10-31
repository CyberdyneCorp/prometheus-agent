//! The file side of an execution backend (spec: execution-environments,
//! File tools route through the backend).
//!
//! File tools never touch the filesystem themselves: they go through the
//! session's [`FileBackend`]. Only a backend whose descriptor says
//! `uses_host_filesystem` reaches host paths, and [`LocalBackend`] (the
//! only one in M1) checks every host access against the session's
//! `fs.read`/`fs.write` capabilities first. Container and remote backends
//! (M2) implement the same trait inside their environment.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pmt_core::capability::{Access, CapabilitySet, Places};

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A character device (`/dev/zero`, a terminal).
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A named pipe.
    Fifo,
    /// A Unix socket.
    Socket,
    /// Anything else.
    Other,
}

/// What [`FileBackend::metadata`] reports (symbolic links followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    /// The kind of the path.
    pub kind: Kind,
    /// Size in bytes.
    pub len: u64,
    /// Last modification time, when the platform reports one.
    pub modified: Option<SystemTime>,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's file name.
    pub name: String,
    /// What it is (links followed; a dangling link is `Other`).
    pub kind: Kind,
    /// Size in bytes (0 for directories).
    pub len: u64,
}

/// File operations of one execution backend. Paths are absolute in the
/// backend's own namespace.
pub trait FileBackend: Send + Sync {
    /// The backend name (`local`).
    fn name(&self) -> &str;
    /// Whether paths are host paths (then capability checks apply).
    fn uses_host_filesystem(&self) -> bool;
    /// Metadata of `path`, following links.
    ///
    /// # Errors
    ///
    /// `NotFound`, `PermissionDenied` (also for a capability refusal).
    fn metadata(&self, path: &Path) -> io::Result<Meta>;
    /// The whole content of `path`.
    ///
    /// # Errors
    ///
    /// As [`FileBackend::metadata`].
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Replaces (or creates, with missing parents) `path` atomically.
    ///
    /// # Errors
    ///
    /// As [`FileBackend::metadata`].
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Removes the file `path`.
    ///
    /// # Errors
    ///
    /// As [`FileBackend::metadata`].
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// The entries of directory `path`, sorted by name.
    ///
    /// # Errors
    ///
    /// As [`FileBackend::metadata`].
    fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>>;
    /// Checks that a host path may be accessed (always `Ok` for a backend
    /// that does not use the host filesystem).
    ///
    /// # Errors
    ///
    /// `PermissionDenied` naming the missing capability.
    fn check(&self, access: Access, path: &Path) -> io::Result<()>;
}

/// The local backend: host paths under the session's capability set.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    capabilities: CapabilitySet,
    places: Places,
}

impl LocalBackend {
    /// A backend allowing what `capabilities` (the session's effective set)
    /// grants, with placeholders resolved by `places`.
    #[must_use]
    pub fn new(capabilities: CapabilitySet, places: Places) -> Self {
        Self {
            capabilities,
            places,
        }
    }
}

impl FileBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local"
    }

    fn uses_host_filesystem(&self) -> bool {
        true
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.check(Access::Read, path)?;
        fs::metadata(path).map(|m| meta_of(&m))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.check(Access::Read, path)?;
        fs::read(path)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.check(Access::Write, path)?;
        write_atomic(path, bytes)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.check(Access::Write, path)?;
        fs::remove_file(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.check(Access::Read, path)?;
        let mut entries: Vec<Entry> = fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|entry| {
                let meta = fs::metadata(entry.path()).ok();
                Entry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: meta.as_ref().map_or(Kind::Other, |m| meta_of(m).kind),
                    len: meta.filter(fs::Metadata::is_file).map_or(0, |m| m.len()),
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn check(&self, access: Access, path: &Path) -> io::Result<()> {
        self.capabilities
            .require_path(access, path, &self.places)
            .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err.message()))
    }
}

fn meta_of(meta: &fs::Metadata) -> Meta {
    Meta {
        kind: kind_of(meta.file_type()),
        len: meta.len(),
        modified: meta.modified().ok(),
    }
}

#[cfg(unix)]
fn kind_of(file_type: fs::FileType) -> Kind {
    use std::os::unix::fs::FileTypeExt as _;
    if file_type.is_file() {
        Kind::File
    } else if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_char_device() {
        Kind::CharDevice
    } else if file_type.is_block_device() {
        Kind::BlockDevice
    } else if file_type.is_fifo() {
        Kind::Fifo
    } else if file_type.is_socket() {
        Kind::Socket
    } else {
        Kind::Other
    }
}

#[cfg(not(unix))]
fn kind_of(file_type: fs::FileType) -> Kind {
    if file_type.is_file() {
        Kind::File
    } else if file_type.is_dir() {
        Kind::Dir
    } else {
        Kind::Other
    }
}

/// Writes through a temporary sibling renamed over `path`, so readers see
/// the old or the new content, never a torn file. An existing file keeps
/// its permissions.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Writing through a link updates its target instead of replacing the
    // link with a regular file.
    let is_link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_link {
        fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let path = target.as_path();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    fs::create_dir_all(&parent)?;
    let permissions = fs::metadata(path).ok().map(|m| m.permissions());
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let temp = parent.join(format!(".{name}.pmt-{}.tmp", ulid::Ulid::generate()));
    let outcome = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = permissions {
            fs::set_permissions(&temp, permissions)?;
        }
        fs::rename(&temp, path)
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    outcome
}
