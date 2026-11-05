//! A plugin's persistent data: `<profile_home>/plugin-data/<id>/` of the
//! instance's own profile (spec: plugin-system, Plugin state and profile
//! scope). Keys live as files under `kv/` (hex-encoded names, so any key
//! is a safe file name) and plugin files under `files/`; a path that is
//! absolute, climbs with `..` or passes through a symbolic link is refused,
//! so one profile's instance can never reach another profile's data.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

/// The longest key, in bytes.
const MAX_KEY_BYTES: usize = 256;

/// One plugin's data directory in one profile.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// `<profile_dir>/plugin-data/<plugin>`.
    #[must_use]
    pub fn new(profile_dir: &Path, plugin: &str) -> Self {
        Self {
            root: profile_dir.join("plugin-data").join(plugin),
        }
    }

    /// The directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The value of `key`.
    ///
    /// # Errors
    ///
    /// An invalid key; an I/O failure.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StateError> {
        match std::fs::read(self.key_path(key)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StateError::Io(e.to_string())),
        }
    }

    /// Stores `value` under `key`.
    ///
    /// # Errors
    ///
    /// An invalid key; an I/O failure.
    pub fn set(&self, key: &str, value: &[u8]) -> Result<(), StateError> {
        let path = self.key_path(key)?;
        write_atomic(&path, value)
    }

    /// Removes `key` (absent is fine).
    ///
    /// # Errors
    ///
    /// An invalid key; an I/O failure.
    pub fn delete(&self, key: &str) -> Result<(), StateError> {
        match std::fs::remove_file(self.key_path(key)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(StateError::Io(e.to_string()))
            }
            _ => Ok(()),
        }
    }

    /// Every key, sorted.
    ///
    /// # Errors
    ///
    /// An I/O failure.
    pub fn keys(&self) -> Result<Vec<String>, StateError> {
        let entries = match std::fs::read_dir(self.root.join("kv")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StateError::Io(e.to_string())),
        };
        let mut keys: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| unhex(&e.file_name().to_string_lossy()))
            .collect();
        keys.sort();
        Ok(keys)
    }

    /// Reads `files/<path>`.
    ///
    /// # Errors
    ///
    /// An invalid path; an I/O failure.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, StateError> {
        std::fs::read(self.file_path(path)?).map_err(|e| StateError::Io(format!("{path}: {e}")))
    }

    /// Writes `files/<path>`, creating parent directories.
    ///
    /// # Errors
    ///
    /// An invalid path; an I/O failure.
    pub fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), StateError> {
        write_atomic(&self.file_path(path)?, contents)
    }

    fn key_path(&self, key: &str) -> Result<PathBuf, StateError> {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(StateError::Invalid(format!(
                "state keys are 1 to {MAX_KEY_BYTES} bytes"
            )));
        }
        let dir = self.root.join("kv");
        self.check_inside(&dir)?;
        Ok(dir.join(hex(key)))
    }

    fn file_path(&self, path: &str) -> Result<PathBuf, StateError> {
        let relative = Path::new(path);
        let clean = !path.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !clean {
            return Err(StateError::Invalid(format!(
                "'{path}' must be a relative path inside the plugin's files"
            )));
        }
        let full = self.root.join("files").join(relative);
        self.check_inside(full.parent().unwrap_or(&self.root))?;
        Ok(full)
    }

    /// Refuses a symbolic link anywhere from the data root down to `dir`.
    fn check_inside(&self, dir: &Path) -> Result<(), StateError> {
        let mut current = self.root.clone();
        let below = dir.strip_prefix(&self.root).unwrap_or(Path::new(""));
        for part in std::iter::once(Path::new(""))
            .chain(below.components().map(|c| Path::new(c.as_os_str())))
        {
            current.push(part);
            if std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(StateError::Invalid(format!(
                    "{} is a symbolic link",
                    current.display()
                )));
            }
        }
        Ok(())
    }
}

/// Why a state operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// A bad key or path.
    Invalid(String),
    /// The filesystem failed.
    Io(String),
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let io = |e: std::io::Error| StateError::Io(e.to_string());
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn hex(key: &str) -> String {
    key.bytes().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(name: &str) -> Option<String> {
    if !name.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..name.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(name.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_files_round_trip_inside_the_plugin_directory() {
        let profile = tempfile::tempdir().unwrap();
        let state = StateDir::new(profile.path(), "notes");
        assert_eq!(state.get("k").unwrap(), None);
        state.set("k", b"v").unwrap();
        state.set("a/b ../c", b"w").unwrap();
        assert_eq!(state.get("k").unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(state.keys().unwrap(), vec!["a/b ../c", "k"]);
        state.delete("k").unwrap();
        assert_eq!(state.keys().unwrap(), vec!["a/b ../c"]);
        state.write_file("dir/x.txt", b"hello").unwrap();
        assert_eq!(state.read_file("dir/x.txt").unwrap(), b"hello");
        assert!(
            profile
                .path()
                .join("plugin-data/notes/files/dir/x.txt")
                .exists()
        );
        for bad in ["../x", "/etc/passwd", "", "a/../../x"] {
            assert!(
                matches!(state.write_file(bad, b""), Err(StateError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_out_of_the_data_directory_is_refused() {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = StateDir::new(profile.path(), "notes");
        std::fs::create_dir_all(state.root().join("files")).unwrap();
        std::os::unix::fs::symlink(outside.path(), state.root().join("files/out")).unwrap();
        assert!(matches!(
            state.write_file("out/x", b"1"),
            Err(StateError::Invalid(_))
        ));
        assert!(!outside.path().join("x").exists());
    }
}
