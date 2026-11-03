//! Reads one plugin file for the threat scan in bounded memory, so every
//! byte of it is scanned however large it is.
//!
//! The file is read in chunks and handed out up to its last complete line;
//! the partial line carries into the next chunk. Patterns match within one
//! line, so none can straddle two segments and line numbers stay exact.
//!
//! A file that is not wholly UTF-8 is still scanned: one stray byte does not
//! stop `sh` from running the lines around it, so its segments are decoded
//! lossily (invalid bytes become U+FFFD; `\n` is never part of a multi-byte
//! sequence, so segment boundaries and line numbers are unaffected). A line
//! that outgrows [`MAX_LINE_BYTES`] while it is read cannot be held whole,
//! so the scan fails on it (closed) instead of cutting it.

use std::borrow::Cow;
use std::io::Read as _;
use std::path::Path;

/// Bytes read per chunk.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Lines up to this long are always scanned whole; a line still unfinished
/// past it fails the scan.
pub(super) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// The first bytes of a WebAssembly module or component.
const WASM_MAGIC: &[u8] = b"\0asm";

/// What a file turned out to be. Either way every line was handed out.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// UTF-8 text.
    Text,
    /// Not wholly UTF-8: the invalid bytes were handed out as U+FFFD.
    Lossy,
}

/// Whether `path` starts with the WebAssembly magic number.
///
/// # Errors
///
/// The file cannot be read.
pub(super) fn is_wasm(path: &Path) -> Result<bool, String> {
    let mut head = [0; WASM_MAGIC.len()];
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    match file.read_exact(&mut head) {
        Ok(()) => Ok(head == WASM_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Hands `visit` each run of whole lines of `path` with the number of
/// lines before it.
///
/// # Errors
///
/// The file cannot be read, a line outgrew [`MAX_LINE_BYTES`], or `visit`
/// failed.
pub(super) fn segments(
    path: &Path,
    mut visit: impl FnMut(&str, usize) -> Result<(), String>,
) -> Result<Kind, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut chunk = vec![0; CHUNK_BYTES];
    let mut pending = Vec::new();
    let mut lines_before = 0;
    let mut kind = Kind::Text;
    loop {
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read]);
        if let Some(end) = pending.iter().rposition(|&b| b == b'\n') {
            let text = decode(&pending[..=end], &mut kind);
            visit(&text, lines_before)?;
            lines_before += end_count(&text);
            pending.drain(..=end);
        }
        if pending.len() > MAX_LINE_BYTES {
            return Err(format!(
                "line {} is longer than {} MiB, too long to scan",
                lines_before + 1,
                MAX_LINE_BYTES / (1024 * 1024)
            ));
        }
    }
    if !pending.is_empty() {
        visit(&decode(&pending, &mut kind), lines_before)?;
    }
    Ok(kind)
}

/// `bytes` as text, lossily (marking `kind`) when they are not UTF-8.
fn decode<'a>(bytes: &'a [u8], kind: &mut Kind) -> Cow<'a, str> {
    std::str::from_utf8(bytes).map_or_else(
        |_| {
            *kind = Kind::Lossy;
            String::from_utf8_lossy(bytes)
        },
        Cow::Borrowed,
    )
}

fn end_count(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(bytes: &[u8]) -> (Result<Kind, String>, Vec<(String, usize)>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, bytes).unwrap();
        let mut seen = Vec::new();
        let kind = segments(&path, |text, before| {
            seen.push((text.to_owned(), before));
            Ok(())
        });
        (kind, seen)
    }

    #[test]
    fn every_line_of_a_large_file_is_handed_out_with_its_line_number() {
        let pad = "# pad\n".repeat(CHUNK_BYTES / 3);
        let text = format!("{pad}last line");
        let (kind, seen) = collect(text.as_bytes());
        assert_eq!(kind, Ok(Kind::Text));
        assert!(seen.len() > 1, "read in more than one chunk");
        let joined: String = seen.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, text);
        let (last, before) = seen.last().unwrap();
        assert_eq!(before + last.lines().count(), text.lines().count());
    }

    #[test]
    fn a_file_that_is_not_utf8_is_still_handed_out_line_by_line() {
        // Regression: one invalid byte made the whole file "binary" and its
        // lines were never scanned, though `sh` runs them.
        let mut bytes = "ok\n".repeat(CHUNK_BYTES).into_bytes();
        bytes.extend_from_slice(b"# \xff\nrun me\n");
        let (kind, seen) = collect(&bytes);
        assert_eq!(kind, Ok(Kind::Lossy));
        let joined: String = seen.iter().map(|(t, _)| t.as_str()).collect();
        assert!(joined.ends_with("ok\n# \u{FFFD}\nrun me\n"));
        let (last, before) = seen.last().unwrap();
        assert_eq!(before + last.lines().count(), CHUNK_BYTES + 2);
    }

    #[test]
    fn only_a_file_starting_with_the_wasm_magic_is_wasm() {
        let dir = tempfile::tempdir().unwrap();
        for (bytes, wasm) in [
            (&b"\0asm\x01\0\0\0"[..], true),
            (b"\0as", false),
            (b"#!/bin/sh\n\0asm", false),
        ] {
            let path = dir.path().join("f");
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(is_wasm(&path), Ok(wasm), "{bytes:?}");
        }
    }

    #[test]
    fn an_overlong_line_fails_the_scan() {
        let mut bytes = b"first\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES + 1));
        let (kind, _) = collect(&bytes);
        assert_eq!(
            kind,
            Err("line 2 is longer than 8 MiB, too long to scan".to_owned())
        );
        let mut binary = b"first\n\xff".to_vec();
        binary.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES + 1));
        assert_eq!(
            collect(&binary).0,
            Err("line 2 is longer than 8 MiB, too long to scan".to_owned())
        );
    }
}
