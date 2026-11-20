//! The harness tree and its on-disk versions (spec: harness-evolution,
//! Harness tree and on-disk layout).
//!
//! A harness is the editable text around the fixed model: a mapping from
//! normalized relative POSIX paths to UTF-8 text. Every version lives under
//! `<profile>/harness/versions/<version_id>/tree/` beside its
//! `version.toml`, and `<profile>/harness/ACTIVE` names the version new
//! sessions load. It lives here, with the profile layout, because a fresh
//! profile is created with its built-in version `h1`; what may be *in* a
//! tree (surface, caps, validation) is `pmt-evolve`'s.

mod versions;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub use versions::{HarnessDir, Source, VersionInfo};

/// The always-on files (components c1-c4), in prompt order.
pub const ALWAYS_ON: [&str; 4] = [
    "SYSTEM.md",
    "GUARDRAILS.md",
    "MEMORY_POLICY.md",
    "TOOL_GUIDANCE.md",
];

/// The hooks file (component c8).
pub const HOOKS_FILE: &str = "hooks.toml";

/// `<profile>/harness`.
#[must_use]
pub fn dir(profile_dir: &Path) -> std::path::PathBuf {
    profile_dir.join("harness")
}

/// Normalizes a tree key: strips leading `./`, turns `\` into `/`, and
/// refuses absolute paths, `..` and empty segments and NUL bytes.
///
/// # Errors
///
/// Why the path is refused.
pub fn normalize(path: &str) -> Result<String, String> {
    if path.contains('\0') {
        return Err("contains a NUL byte".to_owned());
    }
    let unified = path.replace('\\', "/");
    if unified.starts_with('/') || unified.get(1..2) == Some(":") {
        return Err("is absolute".to_owned());
    }
    let mut rest = unified.as_str();
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    let mut segments = Vec::new();
    for segment in rest.split('/') {
        match segment {
            ".." => return Err("leaves the harness (`..`)".to_owned()),
            "" | "." => return Err("has an empty or `.` segment".to_owned()),
            other => segments.push(other),
        }
    }
    Ok(segments.join("/"))
}

/// A harness tree: path → text, in path order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// The files. Keys are normalized only once the tree validated.
    pub files: BTreeMap<String, String>,
}

impl Tree {
    /// The built-in harness: the four always-on files, empty.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            files: ALWAYS_ON
                .iter()
                .map(|p| ((*p).to_owned(), String::new()))
                .collect(),
        }
    }

    /// The text of `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    /// The first 16 hex characters of SHA-256 over every file in path
    /// order as `path \0 bytes \0`.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hash = Sha256::new();
        for (path, text) in &self.files {
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(text.as_bytes());
            hash.update([0]);
        }
        let mut hex = String::with_capacity(16);
        for byte in &hash.finalize()[..8] {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// The always-on text in characters.
    #[must_use]
    pub fn always_on_chars(&self) -> usize {
        ALWAYS_ON
            .iter()
            .filter_map(|p| self.get(p))
            .map(|t| t.chars().count())
            .sum()
    }

    /// Reads the tree under `root`. Symlinks and files that are not UTF-8
    /// are reported in the second value instead of read: a tree that
    /// holds one never validates.
    ///
    /// # Errors
    ///
    /// The directory cannot be walked.
    pub fn read(root: &Path) -> std::io::Result<(Self, Vec<String>)> {
        let mut tree = Self::default();
        let mut problems = Vec::new();
        read_into(root, root, &mut tree, &mut problems)?;
        Ok((tree, problems))
    }

    /// Writes every file under `root` (which must not exist yet or be
    /// empty).
    ///
    /// # Errors
    ///
    /// A directory or file cannot be created.
    pub fn write(&self, root: &Path) -> std::io::Result<()> {
        pmt_core::private_fs::create_dir(root)?;
        for (path, text) in &self.files {
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, text)?;
        }
        Ok(())
    }
}

fn read_into(
    root: &Path,
    dir: &Path,
    tree: &mut Tree,
    problems: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            problems.push(format!("invalid path '{relative}': is a symlink"));
        } else if kind.is_dir() {
            read_into(root, &path, tree, problems)?;
        } else {
            match String::from_utf8(std::fs::read(&path)?) {
                Ok(text) => {
                    tree.files.insert(relative, text);
                }
                Err(_) => problems.push(format!("{relative}: not UTF-8 text")),
            }
        }
    }
    Ok(())
}

/// Whether `path` names a `.py` or `.sh` script (the surface's lowercase
/// extensions).
#[must_use]
pub fn is_script(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e == "py" || e == "sh")
}

/// What a valid tree holds (`version.toml` and `harness log`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Census {
    /// Characters of always-on text.
    pub always_on_chars: usize,
    /// Files.
    pub files: usize,
    /// Skills.
    pub skills: usize,
    /// Subagent definitions.
    pub agents: usize,
    /// Scripts (`scripts/` and skill scripts).
    pub scripts: usize,
    /// Hook entries of `hooks.toml`.
    pub hooks: usize,
}

impl Census {
    /// Counts what `tree` holds.
    #[must_use]
    pub fn of(tree: &Tree) -> Self {
        let paths = || tree.files.keys().map(String::as_str);
        let skills = paths()
            .filter(|p| p.starts_with("skills/") && p.ends_with("/SKILL.md"))
            .count();
        let agents = paths()
            .filter(|p| p.starts_with("agents/") && p.ends_with("/AGENT.md"))
            .count();
        let scripts = paths()
            .filter(|p| (p.starts_with("scripts/") || p.contains("/scripts/")) && is_script(p))
            .count();
        let hooks = tree
            .get(HOOKS_FILE)
            .and_then(|text| toml::from_str::<toml::Table>(text).ok())
            .and_then(|table| {
                table
                    .get("hooks")
                    .and_then(toml::Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or(0);
        Self {
            always_on_chars: tree.always_on_chars(),
            files: tree.files.len(),
            skills,
            agents,
            scripts,
            hooks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_strips_dot_slash_and_refuses_escapes() {
        assert_eq!(normalize("./SYSTEM.md").unwrap(), "SYSTEM.md");
        assert_eq!(normalize("././scripts/a.py").unwrap(), "scripts/a.py");
        assert!(normalize("../config.toml").unwrap_err().contains(".."));
        assert!(normalize("skills/../x").is_err());
        assert!(normalize("/etc/passwd").unwrap_err().contains("absolute"));
        assert!(normalize("C:/x").is_err());
        assert!(normalize("a\0b").unwrap_err().contains("NUL"));
    }

    #[test]
    fn the_fingerprint_hashes_paths_and_bytes_in_order() {
        let mut expected = Sha256::new();
        for path in [
            "GUARDRAILS.md",
            "MEMORY_POLICY.md",
            "SYSTEM.md",
            "TOOL_GUIDANCE.md",
        ] {
            expected.update(path.as_bytes());
            expected.update([0, 0]);
        }
        let hex = expected.finalize()[..8]
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            });
        assert_eq!(Tree::builtin().fingerprint(), hex);
        let mut edited = Tree::builtin();
        edited.files.insert("SYSTEM.md".into(), "x".into());
        assert_ne!(edited.fingerprint(), hex);
    }
}
