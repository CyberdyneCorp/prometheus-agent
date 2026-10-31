//! Which directories are skills (spec: skills, Skill directory layout and
//! scanner exclusions).
//!
//! A directory is a skill when it directly contains `SKILL.md`. Everything
//! below a skill belongs to it: a `SKILL.md` in its `references/` (or any
//! other subdirectory) is an example, never a standalone skill. Version
//! control, archive, lock, dependency and tool-cache directories are not
//! walked at all. Detection works on a list of relative file paths, so the
//! embedded bundled tree and a directory on disk are scanned alike.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// Supporting directories of a skill.
pub const SUPPORT_DIRS: [&str; 5] = ["references", "templates", "scripts", "assets", "evals"];

/// Directories never walked: version control, Prometheus' own bookkeeping
/// (archive, backups, locks, staged writes), dependencies, virtualenvs and
/// tool caches.
pub const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".github",
    ".archive",
    ".backups",
    ".locks",
    ".pending",
    "node_modules",
    "site-packages",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".cache",
];

/// Deepest directory level walked below a root (symlink loops end here).
const MAX_DEPTH: usize = 16;

/// Whether a directory named `name` is skipped.
#[must_use]
pub fn excluded(name: &str) -> bool {
    EXCLUDED_DIRS.contains(&name)
}

/// The skill directories among `files` (relative, `/`-separated), in
/// sorted order; `""` is the root itself.
#[must_use]
pub fn skill_dirs<'a>(files: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let candidates: BTreeSet<&str> = files
        .into_iter()
        .filter(|f| !f.split('/').any(excluded))
        .filter_map(|f| match f.rsplit_once('/') {
            Some((dir, SKILL_FILE)) => Some(dir),
            None if f == SKILL_FILE => Some(""),
            _ => None,
        })
        .collect();
    candidates
        .iter()
        .filter(|dir| !candidates.iter().any(|outer| contains(outer, dir)))
        .map(|dir| (*dir).to_owned())
        .collect()
}

/// Whether `inner` is strictly below `outer`.
fn contains(outer: &str, inner: &str) -> bool {
    if outer == inner {
        return false;
    }
    outer.is_empty()
        || inner
            .strip_prefix(outer)
            .is_some_and(|r| r.starts_with('/'))
}

/// Every file below `root` as a relative `/`-separated path, skipping
/// excluded directories; symlinks are followed (a linked skill directory
/// is common) up to a fixed depth.
#[must_use]
pub fn walk(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    walk_into(root, "", 0, &mut seen, &mut files);
    files.sort();
    files
}

fn walk_into(
    dir: &Path,
    prefix: &str,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    files: &mut Vec<String>,
) {
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if depth > MAX_DEPTH || !seen.insert(canonical) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() && !excluded(&name) => {
                walk_into(&path, &rel, depth + 1, seen, files);
            }
            Ok(meta) if meta.is_file() => files.push(rel),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_skill_files_belong_to_the_outer_skill() {
        let files = [
            "foo/SKILL.md",
            "foo/references/example/SKILL.md",
            "foo/scripts/run.sh",
            "dev/git-workflow/SKILL.md",
            "node_modules/pkg/SKILL.md",
            "dev/.git/SKILL.md",
            "README.md",
        ];
        assert_eq!(skill_dirs(files), ["dev/git-workflow", "foo"]);
    }

    #[test]
    fn a_root_that_is_a_skill() {
        assert_eq!(skill_dirs(["SKILL.md", "sub/SKILL.md"]), [""]);
    }
}
