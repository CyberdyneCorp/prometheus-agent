//! Where a skill's files live: a directory on disk or a directory of the
//! embedded bundled tree. Every read of a supporting file goes through
//! [`SkillDir::read`], which refuses anything outside the skill directory
//! (spec: skills, `skills_list` and `skill_view` tools: canonical paths, with
//! symlinks resolved).

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::bundled::Bundled;
use crate::{digest, layout};

/// The refusal text for a path that leaves the skill directory.
pub const OUTSIDE: &str = "file is outside the skill directory";

/// A skill's directory.
#[derive(Clone)]
pub enum SkillDir {
    /// On disk.
    Disk(PathBuf),
    /// In the embedded tree, at `prefix`.
    Bundled {
        /// The tree.
        tree: Arc<Bundled>,
        /// The skill's directory within it.
        prefix: String,
    },
}

impl fmt::Debug for SkillDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The path leaves the skill directory ([`OUTSIDE`]).
    Outside,
    /// No such file.
    Missing,
    /// The file exists but could not be read.
    Io(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Outside => f.write_str(OUTSIDE),
            Self::Missing => f.write_str("no such file in the skill"),
            Self::Io(e) => f.write_str(e),
        }
    }
}

impl SkillDir {
    /// `bundled:<prefix>` or the directory path.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Disk(path) => path.display().to_string(),
            Self::Bundled { prefix, .. } => format!("bundled:{prefix}"),
        }
    }

    /// The directory on disk, if it is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Disk(path) => Some(path),
            Self::Bundled { .. } => None,
        }
    }

    /// Every file of the skill, relative and `/`-separated, sorted.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        match self {
            Self::Disk(path) => layout::walk(path),
            Self::Bundled { tree, prefix } => tree.files_in(prefix),
        }
    }

    /// The content digest of the skill's files.
    #[must_use]
    pub fn digest(&self) -> String {
        match self {
            Self::Disk(path) => digest::dir(path),
            Self::Bundled { tree, prefix } => tree.digest(prefix),
        }
    }

    /// Reads `rel`, refusing absolute paths, `..` and any path whose
    /// canonical form (symlinks resolved) is outside the directory.
    ///
    /// # Errors
    ///
    /// [`ReadError`].
    pub fn read(&self, rel: &str) -> Result<Vec<u8>, ReadError> {
        let relative = Path::new(rel);
        let plain = relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if rel.is_empty() || !plain {
            return Err(ReadError::Outside);
        }
        match self {
            Self::Bundled { tree, prefix } => tree
                .read(prefix, rel.trim_start_matches("./"))
                .map(<[u8]>::to_vec)
                .ok_or(ReadError::Missing),
            Self::Disk(dir) => read_inside(dir, relative),
        }
    }
}

fn read_inside(dir: &Path, rel: &Path) -> Result<Vec<u8>, ReadError> {
    let root = dir
        .canonicalize()
        .map_err(|e| ReadError::Io(format!("{}: {e}", dir.display())))?;
    let target = match dir.join(rel).canonicalize() {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) => return Err(ReadError::Io(e.to_string())),
    };
    if !target.starts_with(&root) {
        return Err(ReadError::Outside);
    }
    if !target.is_file() {
        return Err(ReadError::Missing);
    }
    std::fs::read(&target).map_err(|e| ReadError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_reads_stay_inside_the_prefix() {
        let tree = Arc::new(Bundled::from_files([
            ("a/SKILL.md", "x"),
            ("a/references/r.md", "r"),
            ("b/SKILL.md", "y"),
        ]));
        let dir = SkillDir::Bundled {
            tree,
            prefix: "a".into(),
        };
        assert_eq!(dir.read("references/r.md").unwrap(), b"r");
        assert_eq!(dir.read("../b/SKILL.md"), Err(ReadError::Outside));
        assert_eq!(dir.read("/etc/passwd"), Err(ReadError::Outside));
        assert_eq!(dir.files(), ["SKILL.md", "references/r.md"]);
    }
}
