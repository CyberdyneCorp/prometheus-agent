//! Reads one plugin file for the threat scan in bounded memory, so every
//! byte of it is scanned however large it is.
//!
//! The file is read in chunks and handed out up to its last complete line;
//! the partial line carries into the next chunk. Patterns match within one
//! line, so none can straddle two segments and line numbers stay exact. A
//! file that is not UTF-8 is binary and has no text to scan (as in the
//! skills guard). A text line that outgrows [`MAX_LINE_BYTES`] while it is
//! read cannot be held whole, so the scan fails on it (closed) instead of
//! cutting it.

use std::io::Read as _;
use std::path::Path;

/// Bytes read per chunk.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Text lines up to this long are always scanned whole; a line still
/// unfinished past it fails the scan.
pub(super) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// What a file turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// UTF-8 text: every line was handed out.
    Text,
    /// Not UTF-8: whatever was handed out does not count.
    Binary,
}

/// Hands `visit` each run of whole lines of `path` with the number of
/// lines before it.
///
/// # Errors
///
/// The file cannot be read, it is text with a line that outgrew
/// [`MAX_LINE_BYTES`], or `visit` failed.
pub(super) fn segments(
    path: &Path,
    mut visit: impl FnMut(&str, usize) -> Result<(), String>,
) -> Result<Kind, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut chunk = vec![0; CHUNK_BYTES];
    let mut pending = Vec::new();
    let mut lines_before = 0;
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
            let Ok(text) = std::str::from_utf8(&pending[..=end]) else {
                return Ok(Kind::Binary);
            };
            visit(text, lines_before)?;
            lines_before += end_count(text);
            pending.drain(..=end);
        }
        if pending.len() > MAX_LINE_BYTES {
            return overlong(&mut file, pending, lines_before + 1);
        }
    }
    let Ok(text) = std::str::from_utf8(&pending) else {
        return Ok(Kind::Binary);
    };
    if !text.is_empty() {
        visit(text, lines_before)?;
    }
    Ok(Kind::Text)
}

fn end_count(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

/// The rest of a file whose line `line` is too long to hold: binary if any
/// of it is not UTF-8, otherwise a scan failure.
fn overlong(file: &mut std::fs::File, mut tail: Vec<u8>, line: usize) -> Result<Kind, String> {
    let mut chunk = vec![0; CHUNK_BYTES];
    loop {
        match std::str::from_utf8(&tail) {
            Ok(_) => tail.clear(),
            // An invalid sequence, not one cut at the chunk's end.
            Err(e) if e.error_len().is_some() => return Ok(Kind::Binary),
            Err(e) => {
                tail.drain(..e.valid_up_to());
            }
        }
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        if read == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..read]);
    }
    if !tail.is_empty() {
        return Ok(Kind::Binary);
    }
    Err(format!(
        "line {line} is longer than {} MiB, too long to scan",
        MAX_LINE_BYTES / (1024 * 1024)
    ))
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
    fn a_file_that_is_not_utf8_is_binary_wherever_the_bad_byte_is() {
        let mut bytes = "ok\n".repeat(CHUNK_BYTES).into_bytes();
        bytes.extend_from_slice(b"\xff\n");
        assert_eq!(collect(&bytes).0, Ok(Kind::Binary));
        let mut long = vec![b'a'; MAX_LINE_BYTES + CHUNK_BYTES];
        long.push(0xff);
        assert_eq!(collect(&long).0, Ok(Kind::Binary));
    }

    #[test]
    fn an_overlong_text_line_fails_the_scan() {
        let mut bytes = b"first\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES + 1));
        let (kind, _) = collect(&bytes);
        assert_eq!(
            kind,
            Err("line 2 is longer than 8 MiB, too long to scan".to_owned())
        );
    }
}
