//! Size-rotated, owner-only log files shared by several processes.
//!
//! Before each write the file at the path is checked: a missing file or
//! (on Unix) a different file than the open one means another process
//! rotated it, so it is reopened; a write that would pass the size limit
//! rotates first (`log` -> `log.1` -> ... -> `log.<keep-1>`, the oldest is
//! removed). Two processes rotating at the same instant can lose one
//! generation of old records; current records are never lost.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use pmt_core::private_fs;

/// Open log files by path.
#[derive(Debug)]
pub struct Rotating {
    max_bytes: u64,
    keep: usize,
    open: HashMap<PathBuf, File>,
}

impl Rotating {
    /// Files rotating past `max_bytes`, `keep` files per log in all.
    #[must_use]
    pub fn new(max_bytes: u64, keep: usize) -> Self {
        Self {
            max_bytes,
            keep: keep.max(1),
            open: HashMap::new(),
        }
    }

    /// Appends `line` to the log at `path`, rotating first when needed.
    ///
    /// # Errors
    ///
    /// Any I/O error creating, rotating or writing the file.
    pub fn write(&mut self, path: &Path, line: &[u8]) -> io::Result<()> {
        let size = self.current_size(path)?;
        if size > 0 && size + line.len() as u64 > self.max_bytes {
            self.open.remove(path);
            rotate(path, self.keep)?;
        }
        let file = match self.open.entry(path.to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(open(path)?),
        };
        file.write_all(line)
    }

    /// Size of the file at `path`, dropping the open handle when it no
    /// longer is that file.
    fn current_size(&mut self, path: &Path) -> io::Result<u64> {
        match fs::metadata(path) {
            Ok(meta) => {
                if let Some(file) = self.open.get(path)
                    && !same_file(file, &meta)
                {
                    self.open.remove(path);
                }
                Ok(meta.len())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.open.remove(path);
                Ok(0)
            }
            Err(err) => Err(err),
        }
    }
}

/// `<path>.<n>`.
#[must_use]
pub fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

fn rotate(path: &Path, keep: usize) -> io::Result<()> {
    if keep == 1 {
        return remove_if_present(path);
    }
    remove_if_present(&rotated(path, keep - 1))?;
    for n in (1..keep - 1).rev() {
        rename_if_present(&rotated(path, n), &rotated(path, n + 1))?;
    }
    rename_if_present(path, &rotated(path, 1))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        private_fs::create_dir(dir)?;
    }
    private_fs::append_options().open(path)
}

#[cfg(unix)]
fn same_file(file: &File, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    file.metadata()
        .is_ok_and(|open| open.dev() == meta.dev() && open.ino() == meta.ino())
}

/// Without inode numbers the handle is trusted; a rotation by another
/// process is noticed by the size check of the next write.
#[cfg(not(unix))]
fn same_file(_file: &File, _meta: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{Rotating, rotated};

    #[test]
    fn rotation_happens_at_the_size_limit_and_keeps_the_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/prometheus.log");
        let mut files = Rotating::new(100, 3);
        let line = [b'a'; 39];
        for i in 0..10u8 {
            let mut text = line.to_vec();
            text[0] = b'0' + i;
            text.push(b'\n');
            files.write(&log, &text).unwrap();
        }
        // 40-byte lines: two fit in 100 bytes, the third rotates.
        let current = std::fs::read_to_string(&log).unwrap();
        assert_eq!(current.lines().count(), 2);
        assert!(current.starts_with('8'), "{current}");
        assert!(
            std::fs::read_to_string(rotated(&log, 1))
                .unwrap()
                .starts_with('6')
        );
        assert!(
            std::fs::read_to_string(rotated(&log, 2))
                .unwrap()
                .starts_with('4')
        );
        assert!(!rotated(&log, 3).exists(), "3 files in all");
        for path in [log.clone(), rotated(&log, 1), rotated(&log, 2)] {
            assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        }
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only_and_a_rotation_by_another_writer_is_followed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        let mut first = Rotating::new(1000, 5);
        let mut second = Rotating::new(1000, 5);
        first.write(&log, b"one\n").unwrap();
        second.write(&log, b"two\n").unwrap();
        std::fs::rename(&log, rotated(&log, 1)).unwrap();
        first.write(&log, b"three\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "three\n");
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
