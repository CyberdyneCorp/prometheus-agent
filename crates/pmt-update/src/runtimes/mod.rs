//! The managed runtime store (spec: distribution-and-update, Managed
//! runtime store; Runtime CLI; Runtime leases and garbage collection;
//! Lazy runtime installation policy; Persistent runtime opt-outs).
//!
//! External runtimes (Node, CPython, uv, Chromium) live in one store shared
//! by every profile of a home:
//!
//! ```text
//! <home>/runtimes/<name>/<version>-<sha12>/      an unpacked, verified archive
//!     .prometheus-runtime.json                    its record (digest of every file)
//! <home>/runtimes/<name>/.leases/<version>-<sha12>/<pid>-<id>.lease
//! <home>/runtimes/.downloads/                     partial downloads (resumed)
//! <home>/runtimes/.staging/                       unpacking, before the rename
//! ```
//!
//! A version directory appears only by an atomic rename of a staging
//! directory that was unpacked from an archive whose size and SHA-256
//! matched the index pinned in this binary ([`index`]), so a crash leaves
//! a partial download or a staging directory, never a half runtime.

pub mod extract;
pub mod gc;
pub mod index;
pub mod install;
pub mod lazy;
pub mod lease;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub use index::{Pin, PinnedArtifact, RuntimeIndex};

/// The record inside every version directory.
pub const RECORD_FILE: &str = ".prometheus-runtime.json";
/// `schema` of [`Record`].
pub const RECORD_SCHEMA: &str = "prometheus.runtime.v1";
const DOWNLOADS: &str = ".downloads";
const STAGING: &str = ".staging";

/// What an installed version records about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// [`RECORD_SCHEMA`].
    pub schema: String,
    /// Runtime name.
    pub name: String,
    /// Runtime version.
    pub version: String,
    /// The archive's pinned SHA-256.
    pub sha256: String,
    /// Where the archive was downloaded from.
    pub url: String,
    /// `origin` or `mirror`.
    pub source: String,
    /// RFC 3339 install time.
    pub installed_at: String,
    /// Retries the download needed (spec: Download retry budget).
    pub retries: u32,
    /// Whether the download resumed earlier bytes.
    pub resumed: bool,
    /// The executable, relative to the version directory.
    pub exe: String,
    /// Digest of every unpacked file (`symlink:<target>` for links), by
    /// `/`-separated relative path.
    pub files: BTreeMap<String, String>,
}

/// One installed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its directory.
    pub dir: PathBuf,
    /// Its record.
    pub record: Record,
}

impl Installed {
    /// The runtime's executable.
    #[must_use]
    pub fn exe(&self) -> PathBuf {
        self.dir.join(&self.record.exe)
    }

    /// `<version>-<sha12>`.
    #[must_use]
    pub fn dir_name(&self) -> String {
        file_name(&self.dir)
    }
}

/// One row of `prometheus runtimes list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    /// Runtime name.
    pub name: String,
    /// The version this binary pins.
    pub pinned: String,
    /// Installed version directories.
    pub installed: Vec<ListedVersion>,
    /// Whether `runtimes.exclude` names it (filled in by the caller).
    pub excluded: bool,
}

/// One installed version in [`Listed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListedVersion {
    /// `<version>-<sha12>`.
    pub dir: String,
    /// Whether it is the pinned one.
    pub pinned: bool,
    /// Bytes on disk.
    pub size: u64,
    /// Processes holding a lease on it.
    pub users: usize,
}

/// The verdict of `prometheus runtimes verify` for one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verified {
    /// Runtime name.
    pub name: String,
    /// `<version>-<sha12>`.
    pub dir: String,
    /// The first differing file, when any.
    pub mismatch: Option<String>,
}

/// A home's runtime store.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    index: RuntimeIndex,
    platform: String,
}

impl Store {
    /// The store of `home` with this binary's index, for this host.
    #[must_use]
    pub fn open(home: &Path) -> Self {
        Self::new(home.join("runtimes"), RuntimeIndex::embedded(), platform())
    }

    /// A store at `root` pinned by `index` for `platform` (tests pin
    /// fixture archives this way).
    #[must_use]
    pub fn new(root: PathBuf, index: RuntimeIndex, platform: String) -> Self {
        Self {
            root,
            index,
            platform,
        }
    }

    /// `<home>/runtimes`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The pinned index.
    #[must_use]
    pub fn index(&self) -> &RuntimeIndex {
        &self.index
    }

    /// The host platform key.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// The pin and archive of `name` for this host.
    ///
    /// # Errors
    ///
    /// `runtime.unknown` (2), `runtime.unsupported` (69).
    pub fn pinned(&self, name: &str) -> Result<(&Pin, &PinnedArtifact), Error> {
        self.index.artifact(name, &self.platform)
    }

    /// The store directory names this binary pins, by runtime name.
    #[must_use]
    pub fn pins(&self) -> BTreeMap<String, String> {
        self.index.pins_for(&self.platform)
    }

    /// The pinned version of `name`, when installed.
    #[must_use]
    pub fn installed(&self, name: &str) -> Option<Installed> {
        let (pin, artifact) = self.pinned(name).ok()?;
        read_installed(&self.root.join(name).join(pin.dir_name(artifact)))
    }

    /// Every installed version of `name`, pinned or not.
    #[must_use]
    pub fn installed_versions(&self, name: &str) -> Vec<Installed> {
        version_dirs(&self.root.join(name))
            .iter()
            .filter_map(|dir| read_installed(dir))
            .collect()
    }

    /// Every pinned runtime with its installed versions.
    #[must_use]
    pub fn list(&self) -> Vec<Listed> {
        let pins = self.pins();
        self.index
            .runtimes
            .iter()
            .map(|(name, pin)| Listed {
                name: name.clone(),
                pinned: pin.version.clone(),
                installed: version_dirs(&self.root.join(name))
                    .iter()
                    .map(|dir| ListedVersion {
                        dir: file_name(dir),
                        pinned: pins.get(name) == Some(&file_name(dir)),
                        size: dir_size(dir),
                        users: lease::users(&self.root, name, &file_name(dir)),
                    })
                    .collect(),
                excluded: false,
            })
            .collect()
    }

    /// Re-hashes every installed file against its record.
    #[must_use]
    pub fn verify(&self) -> Vec<Verified> {
        self.index
            .runtimes
            .keys()
            .flat_map(|name| {
                version_dirs(&self.root.join(name))
                    .into_iter()
                    .map(move |dir| Verified {
                        name: name.clone(),
                        dir: file_name(&dir),
                        mismatch: verify_dir(&dir).err(),
                    })
            })
            .collect()
    }

    /// Removes every version of `name` that no process leases; returns the
    /// removed directories and the leased ones that were kept.
    ///
    /// # Errors
    ///
    /// `runtime.unknown` (2); `runtime.io`.
    pub fn remove(&self, name: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>), Error> {
        self.index.pin(name)?;
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for dir in version_dirs(&self.root.join(name)) {
            if lease::users(&self.root, name, &file_name(&dir)) > 0 {
                kept.push(dir);
            } else {
                remove_dir(&dir)?;
                removed.push(dir);
            }
        }
        Ok((removed, kept))
    }

    fn downloads(&self) -> PathBuf {
        self.root.join(DOWNLOADS)
    }

    fn staging(&self) -> PathBuf {
        self.root.join(STAGING)
    }
}

/// This host's platform key: `<os>-<arch>[-musl]`, with `os` one of
/// `darwin`, `linux`, `windows`, `android` and `arch` `x64` or `arm64`.
/// The C library is the host's, not this binary's: the static musl build
/// on a glibc distribution installs glibc runtimes.
#[must_use]
pub fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let musl = if os == "linux" && host_is_musl() {
        "-musl"
    } else {
        ""
    };
    format!("{os}-{arch}{musl}")
}

/// Whether the host's dynamic loader is musl's (Alpine and friends).
fn host_is_musl() -> bool {
    std::fs::read_dir("/lib").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
    })
}

fn read_installed(dir: &Path) -> Option<Installed> {
    let text = std::fs::read_to_string(dir.join(RECORD_FILE)).ok()?;
    let record = serde_json::from_str(&text).ok()?;
    Some(Installed {
        dir: dir.to_owned(),
        record,
    })
}

/// The version directories under a runtime's directory (hidden entries,
/// such as `.leases`, are not versions).
fn version_dirs(runtime_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Digests of every file under `dir` but the record.
pub(crate) fn digest_tree(dir: &Path) -> std::io::Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    files.remove(RECORD_FILE);
    Ok(files)
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        let relative = relative_name(root, &path);
        if kind.is_symlink() {
            let target = std::fs::read_link(&path)?;
            out.insert(relative, format!("symlink:{}", target.to_string_lossy()));
        } else if kind.is_dir() {
            walk(root, &path, out)?;
        } else {
            out.insert(relative, hash_file(&path)?);
        }
    }
    Ok(())
}

fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Lower-case hex SHA-256 of a file, streamed.
///
/// # Errors
///
/// The read error.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0; 1 << 16];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    }))
}

/// `Err(first differing path)` when the files differ from the record.
fn verify_dir(dir: &Path) -> Result<(), String> {
    let record = read_installed(dir)
        .ok_or_else(|| RECORD_FILE.to_owned())?
        .record;
    let actual = digest_tree(dir).map_err(|e| e.to_string())?;
    let differing = record
        .files
        .iter()
        .find(|(path, digest)| actual.get(*path) != Some(*digest))
        .map(|(path, _)| path.clone())
        .or_else(|| {
            actual
                .keys()
                .find(|path| !record.files.contains_key(*path))
                .cloned()
        });
    differing.map_or(Ok(()), Err)
}

/// Bytes of the regular files under `dir`.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn remove_dir(dir: &Path) -> Result<(), Error> {
    std::fs::remove_dir_all(dir).map_err(|e| Error::io(&codes::RUNTIME_IO, dir, &e))
}

#[cfg(test)]
mod tests {
    use super::platform;

    #[test]
    fn platform_names_os_and_arch() {
        let p = platform();
        assert!(
            ["darwin-", "linux-", "windows-", "android-"]
                .iter()
                .any(|os| p.starts_with(os)),
            "{p}"
        );
    }
}
