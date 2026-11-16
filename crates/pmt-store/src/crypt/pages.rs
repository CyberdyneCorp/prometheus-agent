//! Page sealing and the file layouts the encrypting VFS maps I/O onto
//! (spec: session-storage, Encryption at rest option).
//!
//! An encrypted database uses [`PAGE`]-byte pages whose last [`RESERVE`]
//! bytes SQLite leaves alone (its per-page reserved space): a page is
//! sealed in place as `XChaCha20-Poly1305(page[..PAGE-RESERVE])` followed
//! by the 24-byte random nonce and the 16-byte tag, so it keeps its size
//! and every file offset stays what SQLite expects. The same page image
//! appears in three files: the database (page `n` at `(n-1)*PAGE`), the
//! write-ahead log (after a 32-byte header, frames of a 24-byte header and
//! one page) and the rollback journal (page-sized records). A page that is
//! all zeros was never written and reads as zeros.

use chacha20poly1305::{AeadInOut as _, KeyInit as _, Tag, XChaCha20Poly1305, XNonce};

/// The page size of every encrypted database.
pub const PAGE: usize = 4096;
/// The reserved bytes per page: nonce and tag.
pub const RESERVE: usize = NONCE + TAG;
const NONCE: usize = 24;
const TAG: usize = 16;
/// The write-ahead log's header.
pub const WAL_HEADER: u64 = 32;
/// A write-ahead log frame's header.
pub const FRAME_HEADER: u64 = 24;

/// A 256-bit database key.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    /// A key from its bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// A fresh random key.
    ///
    /// # Errors
    ///
    /// The OS random source failed.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
        Ok(Self(bytes))
    }

    /// The bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // Best effort: the key does not outlive its owner in this buffer.
        self.0.iter_mut().for_each(|b| *b = 0);
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// Seals one page in place.
///
/// # Errors
///
/// A page of the wrong size or a failed random source.
pub fn seal(key: &Key, page: &mut [u8]) -> Result<(), String> {
    if page.len() != PAGE {
        return Err(format!("a page is {PAGE} bytes, not {}", page.len()));
    }
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let (body, reserved) = page.split_at_mut(PAGE - RESERVE);
    let tag = XChaCha20Poly1305::new(&(*key.bytes()).into())
        .encrypt_inout_detached(&XNonce::from(nonce), b"", body.into())
        .map_err(|_| "page encryption failed".to_owned())?;
    reserved[..NONCE].copy_from_slice(&nonce);
    reserved[NONCE..].copy_from_slice(&tag);
    Ok(())
}

/// Opens one sealed page in place; an all-zero page stays zeros.
///
/// # Errors
///
/// A page of the wrong size, or one that does not authenticate (another
/// key, or tampered bytes).
pub fn open(key: &Key, page: &mut [u8]) -> Result<(), String> {
    if page.len() != PAGE {
        return Err(format!("a page is {PAGE} bytes, not {}", page.len()));
    }
    if page.iter().all(|b| *b == 0) {
        return Ok(());
    }
    let (body, reserved) = page.split_at_mut(PAGE - RESERVE);
    let nonce: [u8; NONCE] = reserved[..NONCE].try_into().map_err(|_| "short nonce")?;
    let tag: [u8; TAG] = reserved[NONCE..].try_into().map_err(|_| "short tag")?;
    XChaCha20Poly1305::new(&(*key.bytes()).into())
        .decrypt_inout_detached(&XNonce::from(nonce), b"", body.into(), &Tag::from(tag))
        .map_err(|_| "page does not authenticate".to_owned())?;
    reserved.fill(0);
    Ok(())
}

/// How a file's bytes hold page images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The database: pages back to back.
    Database,
    /// The write-ahead log: a header, then frames.
    Wal,
    /// The rollback journal: every page-sized read or write is a page.
    Journal,
}

/// The byte range `[start, start + PAGE)` of each page image that
/// `[offset, offset + len)` touches.
#[must_use]
pub fn pages(layout: Layout, offset: u64, len: usize) -> Vec<u64> {
    let end = offset + len as u64;
    let page = PAGE as u64;
    match layout {
        Layout::Journal => {
            if len == PAGE {
                vec![offset]
            } else {
                Vec::new()
            }
        }
        Layout::Database => {
            let first = offset / page;
            let last = end.div_ceil(page);
            (first..last).map(|n| n * page).collect()
        }
        Layout::Wal => {
            let frame = FRAME_HEADER + page;
            let mut out = Vec::new();
            if end <= WAL_HEADER {
                return out;
            }
            let first = offset.saturating_sub(WAL_HEADER) / frame;
            let mut n = first;
            loop {
                let start = WAL_HEADER + n * frame + FRAME_HEADER;
                if start >= end {
                    break;
                }
                if start + page > offset {
                    out.push(start);
                }
                n += 1;
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_page_opens_with_its_key_only() {
        let key = Key::generate().unwrap();
        let mut page = vec![7u8; PAGE];
        page[PAGE - RESERVE..].fill(0);
        let plain = page.clone();
        seal(&key, &mut page).unwrap();
        assert_ne!(page[..64], plain[..64]);
        let mut other = page.clone();
        assert!(open(&Key::generate().unwrap(), &mut other).is_err());
        open(&key, &mut page).unwrap();
        assert_eq!(page, plain);
        let mut zeros = vec![0u8; PAGE];
        open(&key, &mut zeros).unwrap();
        assert!(zeros.iter().all(|b| *b == 0));
    }

    #[test]
    fn layouts_name_the_page_images_an_io_touches() {
        let p = PAGE as u64;
        assert_eq!(pages(Layout::Database, 0, 100), [0]);
        assert_eq!(pages(Layout::Database, p, PAGE), [p]);
        assert_eq!(pages(Layout::Database, 0, 2 * PAGE), [0, p]);
        // WAL header alone, one frame header, a frame's page, a whole frame.
        assert!(pages(Layout::Wal, 0, 32).is_empty());
        assert!(pages(Layout::Wal, 32, 24).is_empty());
        assert_eq!(pages(Layout::Wal, 56, PAGE), [56]);
        assert_eq!(pages(Layout::Wal, 32, 24 + PAGE), [56]);
        assert_eq!(
            pages(Layout::Wal, 32 + 24 + p, 24 + PAGE),
            [32 + 24 + p + 24]
        );
        assert_eq!(pages(Layout::Journal, 516, PAGE), [516]);
        assert!(pages(Layout::Journal, 512, 4).is_empty());
    }
}
