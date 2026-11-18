//! Encryption at rest (spec: session-storage, Encryption at rest option).
//!
//! With `storage.encryption = "keychain"` or `"passphrase"` the store's
//! pages are sealed by an encrypting SQLite VFS ([`vfs`], [`pages`]) and
//! every blob file is sealed whole ([`seal_blob`]) under one per-profile
//! 256-bit key: a random key kept in the OS keychain (`STATE_DB_KEY`), or
//! one derived with Argon2id from the `STATE_DB_PASSPHRASE` secret and the
//! salt in `state.db.salt`. A store is converted only offline
//! (`prometheus sessions encrypt|decrypt`, [`convert`]). An encrypted store
//! whose key is unavailable fails closed with `store.key_unavailable`
//! (exit 78) and is never replaced by a new empty one.

pub mod convert;
pub mod keys;
pub mod pages;
mod vfs;

use std::path::Path;

use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest as _, Sha256};

pub use pages::Key;

/// The message of a store whose key cannot be had.
pub const KEY_UNAVAILABLE: &str = "state.db is encrypted and its key is unavailable";

/// The first bytes of a plaintext SQLite database.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// A store's encryption: its key and the VFS sealing with it.
#[derive(Debug, Clone)]
pub struct Crypt {
    key: Key,
    vfs: String,
}

impl Crypt {
    /// The encryption of `key`, registering its VFS.
    ///
    /// # Errors
    ///
    /// `store.sqlite` when SQLite refuses the VFS.
    pub fn new(key: Key) -> Result<Self, Error> {
        let vfs = vfs::name_for(&key).map_err(|e| Error::new(&codes::STORE_SQLITE, e))?;
        Ok(Self { key, vfs })
    }

    /// The key.
    #[must_use]
    pub fn key(&self) -> &Key {
        &self.key
    }
}

/// A short public fingerprint of `key` (names its VFS).
#[must_use]
pub fn fingerprint(key: &Key) -> String {
    use std::fmt::Write as _;
    Sha256::digest(key.bytes())
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Opens the database at `path` with `flags`, through the encrypting VFS
/// when `crypt` is set. A new encrypted database gets [`pages::PAGE`]-byte
/// pages with their reserved space before anything is written.
///
/// # Errors
///
/// SQLite's error.
pub fn connect(
    path: &Path,
    flags: OpenFlags,
    crypt: Option<&Crypt>,
) -> rusqlite::Result<Connection> {
    let Some(crypt) = crypt else {
        return Connection::open_with_flags(path, flags);
    };
    let conn = Connection::open_with_flags_and_vfs(path, flags, crypt.vfs.as_str())?;
    // Temporary tables and sorts stay in memory.
    conn.pragma_update(None, "temp_store", "memory")?;
    let fresh: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    if fresh == 0 && !flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        conn.pragma_update(
            None,
            "page_size",
            i64::try_from(pages::PAGE).unwrap_or(4096),
        )?;
        vfs::reserve_bytes(&conn, reserve())?;
    }
    Ok(conn)
}

fn reserve() -> std::ffi::c_int {
    std::ffi::c_int::try_from(pages::RESERVE).unwrap_or(0)
}

/// Opens `path` read-write (creating it), as [`connect`] does.
///
/// # Errors
///
/// SQLite's error.
pub fn open(path: &Path, crypt: Option<&Crypt>) -> rusqlite::Result<Connection> {
    connect(path, OpenFlags::default(), crypt)
}

/// Whether the file at `path` starts like a plaintext SQLite database
/// (`None`: missing or empty).
#[must_use]
pub fn is_plaintext(path: &Path) -> Option<bool> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(SQLITE_MAGIC.len());
    std::fs::File::open(path)
        .ok()?
        .take(SQLITE_MAGIC.len() as u64)
        .read_to_end(&mut head)
        .ok()?;
    if head.is_empty() {
        return None;
    }
    Some(head == SQLITE_MAGIC)
}

/// Whether `key` opens the first page of the database at `path` (a file
/// shorter than a page holds nothing to check and passes).
#[must_use]
pub fn opens_first_page(path: &Path, key: &Key) -> bool {
    use std::io::Read as _;
    let mut page = vec![0u8; pages::PAGE];
    let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut page));
    read.is_err() || pages::is_blank(&page) || pages::open(key, &mut page, 1).is_ok()
}

/// Seals a blob: a random 24-byte nonce, then the ciphertext and tag.
///
/// # Errors
///
/// A failed random source.
pub fn seal_blob(key: &Key, plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let sealed = XChaCha20Poly1305::new(&(*key.bytes()).into())
        .encrypt(&XNonce::from(nonce), plain)
        .map_err(|_| "blob encryption failed".to_owned())?;
    let mut out = nonce.to_vec();
    out.extend(sealed);
    Ok(out)
}

/// Opens a sealed blob.
///
/// # Errors
///
/// Bytes that are not a blob sealed with `key`.
pub fn open_blob(key: &Key, sealed: &[u8]) -> Result<Vec<u8>, String> {
    if sealed.len() < 24 {
        return Err("sealed blob too short".to_owned());
    }
    let (nonce, body) = sealed.split_at(24);
    let nonce: [u8; 24] = nonce.try_into().map_err(|_| "short nonce".to_owned())?;
    XChaCha20Poly1305::new(&(*key.bytes()).into())
        .decrypt(&XNonce::from(nonce), body)
        .map_err(|_| "blob does not authenticate".to_owned())
}

/// Writes a consistent copy of the open database `conn` to `target`
/// (which must not exist), sealed with `to` (plaintext for `None`). The
/// caller then replaces the original atomically.
///
/// # Errors
///
/// SQLite's error.
pub fn convert(conn: &Connection, target: &Path, to: Option<&Crypt>) -> rusqlite::Result<()> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    if to.is_some() && usize::try_from(page_size).ok() != Some(pages::PAGE) {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "page size {page_size} (encryption needs {})",
            pages::PAGE
        )));
    }
    if to.is_some() {
        // The copy is written with the reserved space the seal needs.
        vfs::reserve_bytes(conn, reserve())?;
    }
    let vfs = to.map_or_else(default_vfs, |c| c.vfs.clone());
    let uri = format!("file:{}?vfs={vfs}", percent(&target.to_string_lossy()));
    conn.execute("VACUUM INTO ?1", [uri]).map(drop)
}

fn default_vfs() -> String {
    vfs::default_name()
}

/// `path` with the characters a URI filename treats specially escaped.
fn percent(path: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '%' | '?' | '#' | '&' | '=' | ' ' => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    let _ = write!(out, "%{b:02X}");
                }
            }
            '\\' => out.push('/'),
            _ => out.push(c),
        }
    }
    if cfg!(windows) && !out.starts_with('/') {
        out.insert(0, '/');
    }
    out
}

#[cfg(test)]
mod tests;
