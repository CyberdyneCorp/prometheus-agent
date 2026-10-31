//! Content digests of skill trees: `sha256:<hex>` over every file's
//! relative path, length and bytes in path order, so a renamed, added,
//! removed or edited file changes the digest and nothing else does.

use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::layout;

/// The digest of `(relative path, content)` files (any order).
#[must_use]
pub fn tree<'a>(files: impl IntoIterator<Item = (String, &'a [u8])>) -> String {
    let mut files: Vec<(String, &[u8])> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let hex = hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("sha256:{hex}")
}

/// The digest of the directory `dir` on disk (excluded directories
/// skipped, as the scanner does); unreadable files count as empty.
#[must_use]
pub fn dir(dir: &Path) -> String {
    let files: Vec<(String, Vec<u8>)> = layout::walk(dir)
        .into_iter()
        .map(|rel| {
            let bytes = std::fs::read(dir.join(&rel)).unwrap_or_default();
            (rel, bytes)
        })
        .collect();
    tree(files.iter().map(|(p, b)| (p.clone(), b.as_slice())))
}
