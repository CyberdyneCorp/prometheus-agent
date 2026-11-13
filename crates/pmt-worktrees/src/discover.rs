//! Repository discovery (spec: projects, Repository discovery): the git
//! repositories under the scan roots, found without following symlinks
//! and without entering hidden folders, dependency folders, excluded
//! subtrees or folders that cannot be read. A repository is not searched
//! for nested ones.

use std::path::{Path, PathBuf};

/// Folders that hold dependencies, never projects.
const DEPENDENCY_DIRS: &[&str] = &["node_modules", "target", ".venv", "vendor"];

/// What one scan looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The folders searched; empty is the user's home.
    pub roots: Vec<PathBuf>,
    /// How many levels below a root are searched (a root is level 0).
    pub max_depth: u32,
    /// Subtrees never entered.
    pub exclude: Vec<PathBuf>,
}

impl Scan {
    /// The scan the `projects.repo_scan_*` keys describe, `~` being
    /// `home`.
    #[must_use]
    pub fn from_keys(roots: &[String], max_depth: u32, exclude: &[String], home: &Path) -> Self {
        let roots = if roots.is_empty() {
            vec![home.to_path_buf()]
        } else {
            roots.iter().map(|r| expand(r, home)).collect()
        };
        Self {
            roots,
            max_depth,
            exclude: exclude.iter().map(|e| expand(e, home)).collect(),
        }
    }

    /// The repositories found, sorted, each once.
    #[must_use]
    pub fn run(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for root in &self.roots {
            self.walk(root, 0, &mut found);
        }
        found.sort();
        found.dedup();
        found
    }

    fn walk(&self, dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
        if self.exclude.iter().any(|e| dir.starts_with(e)) {
            return;
        }
        if dir.join(".git").exists() {
            found.push(dir.to_path_buf());
            return;
        }
        if depth >= self.max_depth {
            return;
        }
        // An unreadable folder (permissions, the sandbox) is skipped.
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || DEPENDENCY_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // `file_type` does not follow symlinks.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                self.walk(&entry.path(), depth + 1, found);
            }
        }
    }
}

/// `path` with a leading `~` as `home`.
fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::Scan;

    fn repo(at: &std::path::Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    /// Scenario: Excluded path skipped.
    #[test]
    fn excluded_hidden_dependency_and_deep_folders_are_skipped() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        repo(&h.join("code/app"));
        repo(&h.join("scratch/tmp"));
        repo(&h.join(".hidden/repo"));
        repo(&h.join("code/web/node_modules/dep"));
        repo(&h.join("code/app/nested"));
        repo(&h.join("a/b/c/d/e/too-deep"));
        let scan = Scan::from_keys(&[], 4, &["~/scratch".to_owned()], h);
        assert_eq!(scan.run(), vec![h.join("code/app")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        repo(&outside.path().join("elsewhere"));
        std::os::unix::fs::symlink(outside.path(), home.path().join("link")).unwrap();
        let scan = Scan::from_keys(&["~".to_owned()], 4, &[], home.path());
        assert!(scan.run().is_empty());
    }
}
