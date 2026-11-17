//! The files a plugin adapter reaches during one call: the call's
//! directory only (the task workspace for `bench.prepare`, the run
//! directory for `bench.deliverables` and `bench.score`). A plugin has no
//! filesystem of its own, so this is its whole view: relative paths only,
//! never `..`, an absolute path or a symbolic link.

use std::path::{Path, PathBuf};

use crate::suite::is_relative_inside;

/// One call's directory, if it has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Files {
    root: Option<PathBuf>,
}

impl Files {
    /// The files below `root`.
    #[must_use]
    pub const fn at(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    /// No directory (`bench.describe`, `bench.tasks`...): every call fails.
    #[must_use]
    pub const fn none() -> Self {
        Self { root: None }
    }

    /// The directory, if any.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// `path` below the root, refusing escapes and symbolic links on the
    /// way (an existing link at any level, the last one included).
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let root = self
            .root
            .as_deref()
            .ok_or("this adapter call has no directory")?;
        if !is_relative_inside(path) {
            return Err(format!("{path}: not a relative path inside the directory"));
        }
        let mut at = root.to_path_buf();
        for part in Path::new(path).components() {
            at.push(part);
            if at
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(format!("{path}: symbolic links are refused"));
            }
        }
        Ok(at)
    }

    /// The bytes of `path`.
    ///
    /// # Errors
    ///
    /// Why it cannot be read.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let at = self.resolve(path)?;
        std::fs::read(&at).map_err(|e| format!("{path}: {e}"))
    }

    /// Writes `path`, creating its parents.
    ///
    /// # Errors
    ///
    /// Why it cannot be written.
    pub fn write(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        let at = self.resolve(path)?;
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
        }
        std::fs::write(&at, contents).map_err(|e| format!("{path}: {e}"))
    }

    /// Every file below the root (`/`-separated, sorted); links are not
    /// followed.
    ///
    /// # Errors
    ///
    /// Why it cannot be listed.
    pub fn list(&self) -> Result<Vec<String>, String> {
        let root = self
            .root
            .as_deref()
            .ok_or("this adapter call has no directory")?;
        let mut out = Vec::new();
        walk(root, root, &mut out).map_err(|e| e.to_string())?;
        out.sort();
        Ok(out)
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(parts.join("/"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Files;

    #[test]
    fn a_call_reaches_its_directory_only() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let files = Files::at(dir.path().to_path_buf());
        files.write("a/b.txt", b"x").unwrap();
        assert_eq!(files.read("a/b.txt").unwrap(), b"x");
        assert_eq!(files.list().unwrap(), vec!["a/b.txt".to_owned()]);
        for bad in ["../x", "/etc/passwd", ""] {
            assert!(files.read(bad).is_err(), "{bad}");
            assert!(files.write(bad, b"x").is_err(), "{bad}");
        }
        assert!(Files::none().read("a/b.txt").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", dir.path().join("link")).unwrap();
            assert!(files.read("link/passwd").is_err());
            assert!(files.write("link/x", b"x").is_err());
        }
    }
}
