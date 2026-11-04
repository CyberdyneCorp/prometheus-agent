//! A plugin's bridge log (spec: python-bridge, Bridge diagnostics; Crash
//! isolation and restart policy): `<profile_home>/logs/bridge/<id>.log`,
//! the sidecar's stderr line by line plus lifecycle notes, rotated at
//! 10 MiB with 3 files kept (`<id>.log`, `.1`, `.2`).

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Rotation threshold.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Files kept (the live one included).
pub const KEEP: usize = 3;
/// stderr lines kept in memory for a crash report.
pub const TAIL_LINES: usize = 200;

/// The log of one plugin.
#[derive(Debug)]
pub struct Log {
    path: PathBuf,
    tail: Mutex<VecDeque<String>>,
}

impl Log {
    /// `<profile_dir>/logs/bridge/<plugin>.log`.
    #[must_use]
    pub fn path_of(profile_dir: &Path, plugin: &str) -> PathBuf {
        profile_dir
            .join("logs")
            .join("bridge")
            .join(format!("{plugin}.log"))
    }

    /// The log at `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            tail: Mutex::new(VecDeque::new()),
        }
    }

    /// Its file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one stderr line (also kept for [`tail`](Self::tail)).
    pub fn line(&self, text: &str) {
        {
            let mut tail = self.tail.lock().unwrap_or_else(PoisonError::into_inner);
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(text.to_owned());
        }
        self.append(text);
    }

    /// Appends a lifecycle note.
    pub fn note(&self, text: &str) {
        self.append(&format!("[bridge] {text}"));
    }

    /// The last stderr lines.
    #[must_use]
    pub fn tail(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn append(&self, text: &str) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= MAX_BYTES) {
            rotate(&self.path);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{text}");
        }
    }
}

fn rotate(path: &Path) {
    let numbered = |n: usize| PathBuf::from(format!("{}.{n}", path.display()));
    let _ = std::fs::remove_file(numbered(KEEP - 1));
    for n in (1..KEEP - 1).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rotates_and_keeps_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.log");
        for n in 0..4 {
            std::fs::write(&path, vec![b'x'; usize::try_from(MAX_BYTES).unwrap()]).unwrap();
            Log::new(path.clone()).note(&format!("round {n}"));
        }
        assert!(path.exists());
        assert!(dir.path().join("p.log.1").exists());
        assert!(dir.path().join("p.log.2").exists());
        assert!(!dir.path().join("p.log.3").exists());
    }
}
