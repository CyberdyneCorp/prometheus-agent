//! Capturing foreground output and spilling the full text to a private file
//! (spec: execution-environments, Spilled full output is redacted and
//! private; Foreground result shaping).
//!
//! Output is kept in memory up to [`MEMORY_LIMIT`] bytes. Beyond that the
//! raw bytes stream to `<spill dir>/<id>.raw` (0600, exclusive create) so a
//! huge output never sits in memory. When the command ends, whenever the
//! cleaned output is longer than `tool_output.max_chars`, the full text is
//! written ANSI-stripped and redacted to `<id>.log` (0600, exclusive
//! create) and only then exposed as `full_output_path`. If the raw file
//! cannot be re-read for redaction, both files are deleted and no path is
//! returned. Files older than [`RETENTION`] are pruned.

use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pmt_core::private_fs;
use pmt_core::secret::Redactor;

use super::shape::{self, OutputHook};

/// Output kept in memory before streaming to a raw spill file.
pub const MEMORY_LIMIT: usize = 1 << 20;

/// How long spill files are kept.
pub const RETENTION: Duration = Duration::from_hours(7 * 24);

/// Chunk size used when redacting a raw spill file.
const CHUNK: usize = 1 << 20;

/// Accumulates one command's output.
#[derive(Debug)]
pub struct Capture {
    dir: PathBuf,
    id: String,
    /// Every byte while not spilled; afterwards the first [`MEMORY_LIMIT`].
    head: Vec<u8>,
    raw: Option<File>,
    /// The raw file could not be written: only `head` is left.
    lossy: bool,
    total: u64,
}

impl Capture {
    /// A capture that spills into `dir` (created 0700 on first spill).
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            id: ulid::Ulid::generate().to_string().to_lowercase(),
            head: Vec::new(),
            raw: None,
            lossy: false,
            total: 0,
        }
    }

    /// Appends output bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.raw.is_none() && !self.lossy && self.head.len() + bytes.len() > MEMORY_LIMIT {
            self.start_raw();
        }
        if let Some(raw) = &mut self.raw
            && raw.write_all(bytes).is_err()
        {
            self.raw = None;
            self.lossy = true;
        }
        let room = MEMORY_LIMIT.saturating_sub(self.head.len());
        let keep = if self.raw.is_some() || self.lossy {
            room.min(bytes.len())
        } else {
            bytes.len()
        };
        self.head.extend_from_slice(&bytes[..keep]);
    }

    fn start_raw(&mut self) {
        let opened = private_fs::create_dir(&self.dir)
            .and_then(|()| private_fs::new_file_options().open(self.raw_path()));
        match opened.and_then(|mut file| file.write_all(&self.head).map(|()| file)) {
            Ok(file) => self.raw = Some(file),
            Err(err) => {
                tracing::warn!(
                    "terminal output spill failed: {err}; keeping the first {MEMORY_LIMIT} bytes"
                );
                let _ = fs::remove_file(self.raw_path());
                self.lossy = true;
            }
        }
    }

    fn raw_path(&self) -> PathBuf {
        self.dir.join(format!("{}.raw", self.id))
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(format!("{}.log", self.id))
    }

    /// Bytes seen so far.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Ends the capture: the shaped output and, when it was truncated and
    /// the full text could be written, the spill file path.
    #[must_use]
    pub fn finish(mut self, max_chars: usize, hooks: &[OutputHook], redactor: &Redactor) -> Shaped {
        if let Some(raw) = self.raw.take() {
            drop(raw);
            return self.finish_spilled(max_chars, hooks, redactor);
        }
        let text = shape::clean(&String::from_utf8_lossy(&self.head), hooks, redactor);
        let (output, truncated) = shape::truncate(&text, max_chars);
        let full_output_path = (truncated && !self.lossy)
            .then(|| self.write_log(&text))
            .flatten();
        Shaped {
            output,
            truncated,
            full_output_path,
        }
    }

    fn write_log(&self, text: &str) -> Option<PathBuf> {
        let path = self.log_path();
        let written = private_fs::create_dir(&self.dir)
            .and_then(|()| private_fs::new_file_options().open(&path))
            .and_then(|mut file| file.write_all(text.as_bytes()));
        match written {
            Ok(()) => Some(path),
            Err(err) => {
                tracing::warn!("terminal output spill failed: {err}");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn finish_spilled(
        &self,
        max_chars: usize,
        hooks: &[OutputHook],
        redactor: &Redactor,
    ) -> Shaped {
        let (raw, log) = (self.raw_path(), self.log_path());
        let result = redact_file(&raw, &log, max_chars, hooks, redactor);
        let _ = fs::remove_file(&raw);
        match result {
            Ok(view) => Shaped {
                output: view.render(),
                truncated: true,
                full_output_path: Some(log),
            },
            Err(err) => {
                tracing::warn!("terminal output spill could not be redacted: {err}; dropping it");
                let _ = fs::remove_file(&log);
                let text = shape::clean(&String::from_utf8_lossy(&self.head), hooks, redactor);
                Shaped {
                    output: shape::truncate(&text, max_chars).0,
                    truncated: true,
                    full_output_path: None,
                }
            }
        }
    }
}

/// A shaped foreground output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shaped {
    /// What the model sees.
    pub output: String,
    /// Whether the output was cut.
    pub truncated: bool,
    /// The redacted full output, when it was cut and could be written.
    pub full_output_path: Option<PathBuf>,
}

/// Head and tail of a redacted spill, collected while writing it.
struct View {
    head: String,
    tail: String,
    head_max: usize,
    tail_max: usize,
    total: usize,
}

impl View {
    fn new(max_chars: usize) -> Self {
        let (head_max, tail_max) = shape::split(max_chars);
        Self {
            head: String::new(),
            tail: String::new(),
            head_max,
            tail_max,
            total: 0,
        }
    }

    fn push(&mut self, text: &str) {
        let mut rest = text;
        let head_room = self.head_max - self.head.chars().count().min(self.head_max);
        if head_room > 0 {
            let cut = rest
                .char_indices()
                .nth(head_room)
                .map_or(rest.len(), |(i, _)| i);
            self.head.push_str(&rest[..cut]);
            rest = &rest[cut..];
        }
        self.total += text.chars().count();
        self.tail.push_str(rest);
        let extra = self.tail.chars().count().saturating_sub(self.tail_max);
        if extra > 0 {
            let cut = self
                .tail
                .char_indices()
                .nth(extra)
                .map_or(self.tail.len(), |(i, _)| i);
            self.tail.drain(..cut);
        }
    }

    fn render(&self) -> String {
        let omitted = self.total - self.head.chars().count() - self.tail.chars().count();
        if omitted == 0 {
            return format!("{}{}", self.head, self.tail);
        }
        format!(
            "{}{}{}",
            self.head,
            shape::marker(omitted, self.total),
            self.tail
        )
    }
}

/// Writes `raw` cleaned (hooks, ANSI, redaction) to `log` chunk by chunk.
/// Chunks end at a newline and never inside a PEM block, so redaction
/// sees whole lines and whole keys.
fn redact_file(
    raw: &Path,
    log: &Path,
    max_chars: usize,
    hooks: &[OutputHook],
    redactor: &Redactor,
) -> io::Result<View> {
    let mut reader = BufReader::new(File::open(raw)?);
    let mut out = private_fs::new_file_options().open(log)?;
    let mut view = View::new(max_chars);
    let mut chunk = Vec::with_capacity(CHUNK);
    loop {
        let read = reader.read_until(b'\n', &mut chunk)?;
        let complete =
            read == 0 || (chunk.len() >= CHUNK && !inside_pem(&chunk)) || chunk.len() >= 4 * CHUNK;
        if !complete {
            continue;
        }
        let text = shape::clean(&String::from_utf8_lossy(&chunk), hooks, redactor);
        out.write_all(text.as_bytes())?;
        view.push(&text);
        chunk.clear();
        if read == 0 {
            break;
        }
    }
    out.sync_all()?;
    Ok(view)
}

fn inside_pem(chunk: &[u8]) -> bool {
    let text = String::from_utf8_lossy(chunk);
    match (text.rfind("-----BEGIN "), text.rfind("-----END ")) {
        (Some(begin), Some(end)) => begin > end,
        (Some(_), None) => true,
        _ => false,
    }
}

/// `<profile>/cache/terminal-spill/<session_id>`.
#[must_use]
pub fn session_dir(profile_dir: &Path, session_id: &str) -> PathBuf {
    root(profile_dir).join(session_id)
}

/// `<profile>/cache/terminal-spill`.
#[must_use]
pub fn root(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("terminal-spill")
}

/// Removes spill files last modified more than [`RETENTION`] before `now`
/// under `root`, and the session directories left empty. Returns how many
/// files were removed.
#[allow(clippy::must_use_candidate)] // pruning is the point; the count is informational
pub fn prune(root: &Path, now: SystemTime) -> usize {
    let Ok(sessions) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for session in sessions.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        for file in fs::read_dir(&session).into_iter().flatten().flatten() {
            let old = file
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age > RETENTION)
                });
            if old && fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
        // Only succeeds when empty.
        let _ = fs::remove_dir(&session);
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pmt_core::secret::{Redactor, SecretString};

    use super::{Capture, MEMORY_LIMIT};

    fn dir() -> tempfile::TempDir {
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp");
        fs::create_dir_all(base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    fn redactor() -> Redactor {
        let secret = SecretString::new("hunter2-secret");
        Redactor::new([&secret])
    }

    #[test]
    fn huge_output_streams_to_a_redacted_private_file() {
        let temp = dir();
        let mut capture = Capture::new(temp.path());
        let line = "\u{1b}[32mok\u{1b}[0m hunter2-secret line\n";
        let mut sent = 0;
        while sent < 3 * MEMORY_LIMIT {
            capture.push(line.as_bytes());
            sent += line.len();
        }
        let shaped = capture.finish(1_000, &[], &redactor());
        let path = shaped.full_output_path.expect("spilled");
        let full = fs::read_to_string(&path).unwrap();
        assert!(!full.contains("hunter2-secret") && !full.contains('\u{1b}'));
        assert_eq!(full.lines().count(), sent / line.len());
        assert!(shaped.output.contains("OUTPUT TRUNCATED"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let leftovers: Vec<_> = fs::read_dir(temp.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "the raw file is removed");
    }

    #[test]
    fn an_unreadable_raw_spill_leaves_no_path_and_no_file() {
        let temp = dir();
        let mut capture = Capture::new(temp.path());
        capture.push(&vec![b'x'; MEMORY_LIMIT + 10]);
        // The raw spill cannot be re-read for redaction.
        fs::remove_file(capture.raw_path()).unwrap();
        let log = capture.log_path();
        let shaped = capture.finish(100, &[], &redactor());
        assert_eq!(shaped.full_output_path, None);
        assert!(!log.exists());
        assert!(shaped.output.contains("OUTPUT TRUNCATED"));
    }

    #[test]
    fn small_truncated_output_is_written_whole() {
        let temp = dir();
        let mut capture = Capture::new(temp.path());
        capture.push("a".repeat(300).as_bytes());
        let shaped = capture.finish(100, &[], &redactor());
        let full = fs::read_to_string(shaped.full_output_path.unwrap()).unwrap();
        assert_eq!(full.len(), 300);
        let mut short = Capture::new(temp.path());
        short.push(b"fits");
        assert_eq!(short.finish(100, &[], &redactor()).full_output_path, None);
    }

    #[test]
    fn old_spills_are_pruned() {
        let temp = dir();
        let session = temp.path().join("ses_1");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("a.log"), "x").unwrap();
        let later =
            std::time::SystemTime::now() + super::RETENTION + std::time::Duration::from_mins(1);
        assert_eq!(super::prune(temp.path(), later), 1);
        assert!(!session.exists());
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("b.log"), "x").unwrap();
        assert_eq!(super::prune(temp.path(), std::time::SystemTime::now()), 0);
        assert!(session.join("b.log").exists());
    }
}
