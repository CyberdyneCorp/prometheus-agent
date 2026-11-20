//! Page sealing and the file layouts the encrypting VFS maps I/O onto
//! (spec: session-storage, Encryption at rest option).
//!
//! An encrypted database uses [`PAGE`]-byte pages whose last [`RESERVE`]
//! bytes SQLite leaves alone (its per-page reserved space): a page is
//! sealed in place as `XChaCha20-Poly1305(page[..PAGE-RESERVE])` followed
//! by the 24-byte random nonce and the 16-byte tag, so it keeps its size
//! and every file offset stays what SQLite expects. The page number is
//! the associated data: a sealed page moved to another page's place does
//! not authenticate. The same page image appears in three files: the
//! database (page `n` at `(n-1)*PAGE`), the write-ahead log (after a
//! 32-byte header, frames of a 24-byte header starting with the page
//! number, then the page) and the rollback journal (records of the page
//! number, the page and a checksum). Whether an all-zero page is a page
//! SQLite never wrote is the VFS's call ([`is_blank`]).

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

/// Seals page `number` in place.
///
/// # Errors
///
/// A page of the wrong size or a failed random source.
pub fn seal(key: &Key, page: &mut [u8], number: u32) -> Result<(), String> {
    if page.len() != PAGE {
        return Err(format!("a page is {PAGE} bytes, not {}", page.len()));
    }
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let (body, reserved) = page.split_at_mut(PAGE - RESERVE);
    let tag = XChaCha20Poly1305::new(&(*key.bytes()).into())
        .encrypt_inout_detached(&XNonce::from(nonce), &number.to_be_bytes(), body.into())
        .map_err(|_| "page encryption failed".to_owned())?;
    reserved[..NONCE].copy_from_slice(&nonce);
    reserved[NONCE..].copy_from_slice(&tag);
    Ok(())
}

/// Whether `page` is all zeros (never written, or zeroed).
#[must_use]
pub fn is_blank(page: &[u8]) -> bool {
    page.iter().all(|b| *b == 0)
}

/// Opens sealed page `number` in place.
///
/// # Errors
///
/// A page of the wrong size, or one that does not authenticate as page
/// `number` (another key, another page's place, tampered or blank bytes).
pub fn open(key: &Key, page: &mut [u8], number: u32) -> Result<(), String> {
    if page.len() != PAGE {
        return Err(format!("a page is {PAGE} bytes, not {}", page.len()));
    }
    let (body, reserved) = page.split_at_mut(PAGE - RESERVE);
    let nonce: [u8; NONCE] = reserved[..NONCE].try_into().map_err(|_| "short nonce")?;
    let tag: [u8; TAG] = reserved[NONCE..].try_into().map_err(|_| "short tag")?;
    XChaCha20Poly1305::new(&(*key.bytes()).into())
        .decrypt_inout_detached(
            &XNonce::from(nonce),
            &number.to_be_bytes(),
            body.into(),
            &Tag::from(tag),
        )
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
        seal(&key, &mut page, 3).unwrap();
        assert_ne!(page[..64], plain[..64]);
        let mut other = page.clone();
        assert!(open(&Key::generate().unwrap(), &mut other, 3).is_err());
        open(&key, &mut page, 3).unwrap();
        assert_eq!(page, plain);
        let mut zeros = vec![0u8; PAGE];
        assert!(is_blank(&zeros));
        assert!(
            open(&key, &mut zeros, 1).is_err(),
            "a blank page is no sealed page"
        );
    }

    #[test]
    fn a_sealed_page_does_not_open_in_another_pages_place() {
        let key = Key::generate().unwrap();
        let mut page = vec![7u8; PAGE];
        page[PAGE - RESERVE..].fill(0);
        seal(&key, &mut page, 3).unwrap();
        let mut moved = page.clone();
        assert!(open(&key, &mut moved, 4).is_err());
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
