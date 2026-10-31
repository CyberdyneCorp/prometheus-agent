//! What a session has read and written (spec: execution-environments,
//! `read_file` guards, pagination and dedup; Stale overwrite protection).
//!
//! The tracker remembers, per file, the content hash the session last saw
//! and which lines it has read of that content. A write is safe only when
//! the session has seen every line of exactly what is on disk now. Repeat
//! reads of an unchanged region are counted so the second one gets a stub
//! and later ones a refusal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

/// SHA-256 of a file's bytes.
pub type Hash = [u8; 32];

/// Hashes file content.
#[must_use]
pub fn hash(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

/// Why a write is refused as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// The session never read (all of) the file.
    NotRead,
    /// The file changed on disk since the session last read or wrote it.
    Changed,
}

/// One identical read: same file, range, content and compaction
/// generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadKey {
    /// The resolved path.
    pub path: PathBuf,
    /// Requested first line.
    pub offset: usize,
    /// Requested line count.
    pub limit: usize,
    /// Content hash at read time.
    pub hash: Hash,
    /// Context compaction generation at read time.
    pub generation: u64,
}

#[derive(Debug)]
struct Record {
    hash: Hash,
    total_lines: usize,
    /// Inclusive line ranges read of this content, merged.
    covered: Vec<(usize, usize)>,
}

impl Record {
    fn fully_read(&self) -> bool {
        self.total_lines == 0
            || self
                .covered
                .first()
                .is_some_and(|&(start, end)| start <= 1 && end >= self.total_lines)
    }

    fn cover(&mut self, start: usize, end: usize) {
        self.covered.push((start, end));
        self.covered.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.covered.len());
        for &(s, e) in &self.covered {
            match merged.last_mut() {
                Some(last) if s <= last.1 + 1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.covered = merged;
    }
}

/// The session's read/write history.
#[derive(Debug, Default)]
pub struct Tracker {
    files: HashMap<PathBuf, Record>,
    reads: HashMap<ReadKey, u32>,
}

impl Tracker {
    /// Records that lines `start..=end` of content `hash` were read.
    pub fn note_read(
        &mut self,
        path: &Path,
        hash: Hash,
        total_lines: usize,
        start: usize,
        end: usize,
    ) {
        let record = self.files.entry(path.to_path_buf()).or_insert(Record {
            hash,
            total_lines,
            covered: Vec::new(),
        });
        if record.hash != hash {
            *record = Record {
                hash,
                total_lines,
                covered: Vec::new(),
            };
        }
        if end >= start {
            record.cover(start, end);
        }
    }

    /// Counts one more identical read; returns how many came before it.
    pub fn repeat(&mut self, key: ReadKey) -> u32 {
        let count = self.reads.entry(key).or_insert(0);
        let before = *count;
        *count += 1;
        before
    }

    /// Records that the session wrote `hash` (it knows every line of it).
    pub fn note_written(&mut self, path: &Path, hash: Hash, total_lines: usize) {
        self.files.insert(
            path.to_path_buf(),
            Record {
                hash,
                total_lines,
                covered: vec![(1, total_lines.max(1))],
            },
        );
        self.reads.retain(|key, _| key.path != path);
    }

    /// Why overwriting `path`, whose content now hashes to `current`, would
    /// be stale, if it would.
    #[must_use]
    pub fn stale(&self, path: &Path, current: Hash) -> Option<Stale> {
        match self.files.get(path) {
            None => Some(Stale::NotRead),
            Some(record) if record.hash != current => Some(Stale::Changed),
            Some(record) if !record.fully_read() => Some(Stale::NotRead),
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Stale, Tracker, hash};

    #[test]
    fn a_file_read_in_pages_counts_as_fully_read() {
        let mut tracker = Tracker::default();
        let path = Path::new("/w/a.rs");
        let h = hash(b"x");
        tracker.note_read(path, h, 10, 1, 4);
        assert_eq!(tracker.stale(path, h), Some(Stale::NotRead));
        tracker.note_read(path, h, 10, 5, 10);
        assert_eq!(tracker.stale(path, h), None);
        assert_eq!(tracker.stale(path, hash(b"y")), Some(Stale::Changed));
    }

    #[test]
    fn new_content_resets_what_was_read() {
        let mut tracker = Tracker::default();
        let path = Path::new("/w/a.rs");
        tracker.note_read(path, hash(b"old"), 2, 1, 2);
        tracker.note_read(path, hash(b"new"), 2, 1, 1);
        assert_eq!(tracker.stale(path, hash(b"new")), Some(Stale::NotRead));
        tracker.note_written(path, hash(b"mine"), 3);
        assert_eq!(tracker.stale(path, hash(b"mine")), None);
    }
}
