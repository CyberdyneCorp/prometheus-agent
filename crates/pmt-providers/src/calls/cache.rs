//! The content-addressed model-call cache (spec: replay-and-evaluation,
//! Content-addressed model-call cache; Cache administration and limits).
//!
//! An entry is the normalised event stream of one successful response
//! (with its chunk boundaries) plus its usage, zstd-compressed JSON at
//! `<profile>/cache/model-calls/<first 2 hex>/<hex>.json.zst` (namespace
//! `default`) or `.../namespaces/<namespace>/<first 2 hex>/<hex>.json.zst`,
//! mode 0600. The request itself is not stored, so an entry holds no
//! credentials and no prompt; failed responses never become entries. The
//! file's modification time is its last use: a hit touches it, and
//! eviction above `replay.cache.max_bytes` removes the least recently used
//! entries first.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use pmt_config::schema::{CacheMode, DEFAULT_NAMESPACE, ModelCallCacheConfig};
use serde::{Deserialize, Serialize};

use super::Recorded;

/// The cache directory under a profile.
pub const DIR: &str = "cache/model-calls";

/// Where non-default namespaces live under [`DIR`].
const NAMESPACES: &str = "namespaces";

const SUFFIX: &str = ".json.zst";

/// zstd level: entries are written once and read many times.
const LEVEL: i32 = 3;

/// One stored entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    format: u32,
    request_hash: String,
    namespace: String,
    provider: String,
    model: String,
    created_at: String,
    response: Recorded,
}

/// The cache of one profile, as a session uses it.
#[derive(Debug)]
pub struct ModelCallCache {
    root: PathBuf,
    namespace: String,
    mode: CacheMode,
    max_bytes: u64,
    /// Bytes of all entries, counted once and kept up to date by writes.
    total: Mutex<Option<u64>>,
}

impl ModelCallCache {
    /// The cache under `profile_dir` with `config`; `None` when it is off.
    #[must_use]
    pub fn for_profile(profile_dir: &Path, config: &ModelCallCacheConfig) -> Option<Self> {
        (config.mode != CacheMode::Off).then(|| Self {
            root: profile_dir.join(DIR),
            namespace: config.namespace.clone(),
            mode: config.mode,
            max_bytes: config.max_bytes,
            total: Mutex::new(None),
        })
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    fn path(&self, hash: &str) -> Option<PathBuf> {
        entry_path(&self.root, &self.namespace, hash)
    }

    /// The stored response of `hash`, when the mode reads and there is
    /// one; a hit counts as a use.
    #[must_use]
    pub fn get(&self, hash: &str) -> Option<Recorded> {
        if !self.mode.reads() {
            return None;
        }
        let path = self.path(hash)?;
        let entry = read_entry(&path).ok()?;
        if entry.request_hash != hash {
            return None;
        }
        touch(&path);
        Some(entry.response)
    }

    /// Stores the successful response of `hash` when the mode writes.
    ///
    /// # Errors
    ///
    /// I/O errors writing the entry.
    pub fn put(
        &self,
        hash: &str,
        (provider, model): (&str, &str),
        response: &Recorded,
    ) -> io::Result<()> {
        if !self.mode.writes() {
            return Ok(());
        }
        let Some(path) = self.path(hash) else {
            return Ok(());
        };
        let entry = Entry {
            format: 1,
            request_hash: hash.to_owned(),
            namespace: self.namespace.clone(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            created_at: jiff::Timestamp::now().to_string(),
            response: response.clone(),
        };
        let json = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        let bytes = zstd::encode_all(json.as_slice(), LEVEL)?;
        if let Some(dir) = path.parent() {
            pmt_core::private_fs::create_dir(dir)?;
        }
        let replaced = fs::metadata(&path).map_or(0, |m| m.len());
        pmt_core::private_fs::write_atomic(&path, &bytes)?;
        self.grew(bytes.len() as u64, replaced)
    }

    /// Accounts for a write and evicts when the cache is over its limit.
    fn grew(&self, added: u64, replaced: u64) -> io::Result<()> {
        let mut total = self.total.lock().unwrap_or_else(PoisonError::into_inner);
        let now = match *total {
            Some(bytes) => (bytes + added).saturating_sub(replaced),
            None => entries(&self.root)?.iter().map(|e| e.bytes).sum(),
        };
        *total = Some(if now > self.max_bytes {
            evict(&self.root, self.max_bytes)?
        } else {
            now
        });
        Ok(())
    }
}

/// The entry file of `hash` (`sha256:<64 hex>`) in `namespace`.
fn entry_path(root: &Path, namespace: &str, hash: &str) -> Option<PathBuf> {
    let hex = hash.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        namespace_dir(root, namespace)
            .join(&hex[..2])
            .join(format!("{hex}{SUFFIX}")),
    )
}

fn namespace_dir(root: &Path, namespace: &str) -> PathBuf {
    if namespace == DEFAULT_NAMESPACE {
        root.to_path_buf()
    } else {
        root.join(NAMESPACES).join(namespace)
    }
}

fn read_entry(path: &Path) -> io::Result<Entry> {
    let bytes = fs::read(path)?;
    let json = zstd::decode_all(bytes.as_slice())?;
    serde_json::from_slice(&json).map_err(io::Error::other)
}

fn touch(path: &Path) {
    if let Ok(file) = fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// One entry on disk.
#[derive(Debug, Clone)]
struct OnDisk {
    path: PathBuf,
    namespace: String,
    bytes: u64,
    used: SystemTime,
}

/// Every entry under `root`, of every namespace.
fn entries(root: &Path) -> io::Result<Vec<OnDisk>> {
    let mut out = Vec::new();
    collect(root, DEFAULT_NAMESPACE, &mut out)?;
    if let Ok(names) = fs::read_dir(root.join(NAMESPACES)) {
        for name in names.flatten() {
            let namespace = name.file_name().to_string_lossy().into_owned();
            collect(&name.path(), &namespace, &mut out)?;
        }
    }
    Ok(out)
}

/// The entries of one namespace directory (`<2 hex>/<hex>.json.zst`).
fn collect(dir: &Path, namespace: &str, out: &mut Vec<OnDisk>) -> io::Result<()> {
    let shards = match fs::read_dir(dir) {
        Ok(shards) => shards,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for shard in shards.flatten() {
        let name = shard.file_name();
        if name.len() != 2 || !shard.path().is_dir() {
            continue;
        }
        for file in fs::read_dir(shard.path())?.flatten() {
            let path = file.path();
            let is_entry = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(SUFFIX));
            let Ok(meta) = file.metadata() else { continue };
            if is_entry && meta.is_file() {
                out.push(OnDisk {
                    path,
                    namespace: namespace.to_owned(),
                    bytes: meta.len(),
                    used: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
    }
    Ok(())
}

/// Removes least recently used entries until the cache holds at most
/// `max_bytes`; returns the bytes left.
fn evict(root: &Path, max_bytes: u64) -> io::Result<u64> {
    let mut all = entries(root)?;
    all.sort_by_key(|e| e.used);
    let mut total: u64 = all.iter().map(|e| e.bytes).sum();
    for entry in all {
        if total <= max_bytes {
            break;
        }
        remove(&entry.path)?;
        total -= entry.bytes;
    }
    Ok(total)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Entry count and size of one namespace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Entries.
    pub entries: u64,
    /// Bytes on disk.
    pub bytes: u64,
}

/// What `prometheus cache stats` reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// All namespaces together.
    pub total: Usage,
    /// Per namespace.
    pub namespaces: BTreeMap<String, Usage>,
}

/// The cache's size under `profile_dir`.
///
/// # Errors
///
/// I/O errors reading the cache directory.
pub fn stats(profile_dir: &Path) -> io::Result<Stats> {
    let mut stats = Stats::default();
    for entry in entries(&profile_dir.join(DIR))? {
        for usage in [
            &mut stats.total,
            stats.namespaces.entry(entry.namespace).or_default(),
        ] {
            usage.entries += 1;
            usage.bytes += entry.bytes;
        }
    }
    Ok(stats)
}

/// Removes the entries (of `namespace`, or all) last used longer than
/// `older_than` ago (`None`: every entry); returns what was removed.
///
/// # Errors
///
/// I/O errors reading the directory or removing an entry.
pub fn prune(
    profile_dir: &Path,
    older_than: Option<Duration>,
    namespace: Option<&str>,
) -> io::Result<Usage> {
    let cutoff = older_than.map(|age| SystemTime::now() - age);
    let mut removed = Usage::default();
    for entry in entries(&profile_dir.join(DIR))? {
        let in_scope = namespace.is_none_or(|ns| ns == entry.namespace);
        let due = cutoff.is_none_or(|cutoff| entry.used < cutoff);
        if in_scope && due {
            remove(&entry.path)?;
            removed.entries += 1;
            removed.bytes += entry.bytes;
        }
    }
    Ok(removed)
}
