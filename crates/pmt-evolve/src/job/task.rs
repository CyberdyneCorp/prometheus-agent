//! Freezing a job's task (spec: harness-evolution, Evolution task
//! capture).
//!
//! `task/` holds `task.md` (the statement), `source/` (the workspace the
//! rollouts re-run in) and `task.toml`. A git workspace is captured as its
//! HEAD commit (`repo.toml`) plus `uncommitted.patch`, built through a
//! private index so the user's own index is never touched; any other
//! workspace as a copy of the files the session read or modified. Leak
//! globs are excluded, text is redacted, and the whole snapshot is capped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_core::error::{Error, codes};
use pmt_core::secret::Redactor;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// `task.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMeta {
    /// The experience-library domain.
    pub domain: String,
    /// Toolsets the rollouts get.
    #[serde(default)]
    pub toolsets: Vec<String>,
    /// The model the task ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The execution backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// The session the task came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// SHA-256 (16 hex) over `task.md` and every file of `source/`.
    #[serde(default)]
    pub fingerprint: String,
}

/// What to freeze.
#[derive(Debug, Clone)]
pub struct Capture {
    /// The task statement (`task.md`).
    pub statement: String,
    /// The workspace, if the task has one.
    pub workspace: Option<PathBuf>,
    /// The files to copy from a workspace that is not a git repository:
    /// `None` copies the whole workspace (a task file), `Some` only these
    /// (the files a session read or modified).
    pub files: Option<Vec<PathBuf>>,
    /// `task.toml` (its fingerprint is computed here).
    pub meta: TaskMeta,
    /// `evolve.leak_globs`.
    pub leak_globs: Vec<String>,
    /// `evolve.task.max_source_bytes`.
    pub max_source_bytes: u64,
    /// The profile's redactor.
    pub redactor: Redactor,
}

/// Matches leak globs against full relative paths, leading dots included.
#[derive(Debug, Clone)]
pub struct LeakGlobs(Vec<glob::Pattern>);

impl LeakGlobs {
    /// Compiles `globs` (an unparsable glob is skipped with a warning; the
    /// config check reports it).
    #[must_use]
    pub fn new(globs: &[String]) -> Self {
        Self(
            globs
                .iter()
                .filter_map(|g| match glob::Pattern::new(g) {
                    Ok(p) => Some(p),
                    Err(err) => {
                        tracing::warn!(glob = %g, error = %err, "evolve.leak_globs entry skipped");
                        None
                    }
                })
                .collect(),
        )
    }

    /// Whether the relative POSIX `path` matches a glob.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.0.iter().any(|p| p.matches_with(path, options))
    }
}

/// Freezes `capture` into `dir` (`<job>/task`) and returns `task.toml`.
///
/// # Errors
///
/// `evolve.invalid_task` (exit 65) when the snapshot exceeds the cap or
/// git fails; `harness.io` for file errors.
pub fn capture(dir: &Path, capture: &Capture) -> Result<TaskMeta, Error> {
    let source = dir.join("source");
    std::fs::create_dir_all(&source).map_err(|e| io(&source, &e))?;
    let statement = capture.redactor.redact_all(&capture.statement);
    write(&dir.join("task.md"), statement.as_bytes())?;
    let leaks = LeakGlobs::new(&capture.leak_globs);
    let mut snapshot = Snapshot {
        root: source.clone(),
        leaks,
        redactor: capture.redactor.clone(),
        bytes: 0,
        cap: capture.max_source_bytes,
    };
    if let Some(workspace) = &capture.workspace {
        match git_root(workspace) {
            Some(root) => snapshot.git(&root, dir)?,
            None => snapshot.files(workspace, capture.files.as_deref())?,
        }
    }
    let mut meta = capture.meta.clone();
    meta.fingerprint = fingerprint(dir)?;
    let text = toml::to_string(&meta).map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
    write(&dir.join("task.toml"), text.as_bytes())?;
    Ok(meta)
}

/// Recomputes the fingerprint in `task.toml` after `task.md` was edited.
///
/// # Errors
///
/// `harness.io`.
pub fn refingerprint(dir: &Path) -> Result<(), Error> {
    let path = dir.join("task.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| io(&path, &e))?;
    let mut meta: TaskMeta = toml::from_str(&text)
        .map_err(|e| Error::new(&codes::HARNESS_IO, e.message().to_owned()))?;
    let statement = dir.join("task.md");
    let edited = std::fs::read_to_string(&statement).map_err(|e| io(&statement, &e))?;
    if edited.trim().is_empty() {
        return Err(Error::new(
            &codes::EVOLVE_INVALID_TASK,
            "the task statement is empty",
        ));
    }
    meta.fingerprint = fingerprint(dir)?;
    let text = toml::to_string(&meta).map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
    write(&path, text.as_bytes())
}

struct Snapshot {
    root: PathBuf,
    leaks: LeakGlobs,
    redactor: Redactor,
    bytes: u64,
    cap: u64,
}

impl Snapshot {
    /// Adds `bytes` to the snapshot, failing past the cap.
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes += u64::try_from(bytes).unwrap_or(u64::MAX);
        if self.bytes > self.cap {
            return Err(Error::new(
                &codes::EVOLVE_INVALID_TASK,
                format!(
                    "the task snapshot exceeds evolve.task.max_source_bytes ({} bytes)",
                    self.cap
                ),
            ));
        }
        Ok(())
    }

    /// Text is redacted; other bytes are copied as they are.
    fn redacted(&self, bytes: Vec<u8>) -> Vec<u8> {
        match String::from_utf8(bytes) {
            Ok(text) => self.redactor.redact_all(&text).into_bytes(),
            Err(raw) => raw.into_bytes(),
        }
    }

    /// `repo.toml` and `uncommitted.patch` of the repository at `root`.
    fn git(&mut self, root: &Path, job_task: &Path) -> Result<(), Error> {
        let head = git(root, &["rev-parse", "HEAD"], None)?;
        let index = job_task.join(".capture-index");
        let env = Some(index.as_path());
        let excludes: Vec<String> = self
            .leaks
            .0
            .iter()
            .map(|p| format!(":(exclude,glob){}", p.as_str()))
            .collect();
        let patch = (|| {
            git(root, &["read-tree", "HEAD"], env)?;
            let mut add = vec!["add", "-A", "--", "."];
            add.extend(excludes.iter().map(String::as_str));
            git(root, &add, env)?;
            git(root, &["diff", "--cached", "--binary", "HEAD"], env)
        })();
        let _ = std::fs::remove_file(&index);
        let patch = self.redacted(patch?.into_bytes());
        self.charge(patch.len())?;
        write(&self.root.join("uncommitted.patch"), &patch)?;
        let repo = format!("commit = {:?}\n", head.trim());
        write(&self.root.join("repo.toml"), repo.as_bytes())
    }

    /// Copies `files` (or the whole workspace) under `workspace`.
    fn files(&mut self, workspace: &Path, files: Option<&[PathBuf]>) -> Result<(), Error> {
        let paths = match files {
            Some(files) => files
                .iter()
                .map(|f| {
                    if f.is_absolute() {
                        f.clone()
                    } else {
                        workspace.join(f)
                    }
                })
                .collect(),
            None => walk(workspace),
        };
        let base = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            let Ok(real) = path.canonicalize() else {
                continue;
            };
            let Ok(relative) = real.strip_prefix(&base) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !real.is_file() || self.leaks.matches(&relative) || !seen.insert(relative.clone()) {
                continue;
            }
            let bytes = std::fs::read(&real).map_err(|e| io(&real, &e))?;
            let bytes = self.redacted(bytes);
            self.charge(bytes.len())?;
            let target = self.root.join(&relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
            }
            write(&target, &bytes)?;
        }
        Ok(())
    }
}

/// Every regular file under `dir`, skipping `.git` and symlinks.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && entry.file_name() != ".git" {
            out.extend(walk(&entry.path()));
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    out
}

/// The repository root of `dir`, when it is inside a git work tree.
fn git_root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], None)
        .ok()
        .map(|out| PathBuf::from(out.trim()))
}

fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String, Error> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(|e| {
        Error::new(
            &codes::EVOLVE_INVALID_TASK,
            format!("git could not run: {e}"),
        )
    })?;
    if !output.status.success() {
        return Err(Error::new(
            &codes::EVOLVE_INVALID_TASK,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// SHA-256 (16 hex) over `task.md` and `source/`, path order.
fn fingerprint(dir: &Path) -> Result<String, Error> {
    let mut files = vec![dir.join("task.md")];
    let mut sources = walk(&dir.join("source"));
    sources.sort();
    files.extend(sources);
    let mut hash = Sha256::new();
    for path in files {
        let relative = path.strip_prefix(dir).unwrap_or(&path);
        hash.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update([0]);
        hash.update(std::fs::read(&path).map_err(|e| io(&path, &e))?);
        hash.update([0]);
    }
    Ok(hash.finalize()[..8]
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        }))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(|e| io(path, &e))
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leak_globs_match_full_paths_with_leading_dots() {
        let globs = LeakGlobs::new(&[
            ".answers/**".to_owned(),
            "**/*rubric*".to_owned(),
            "**/expected/**".to_owned(),
        ]);
        assert!(globs.matches(".answers/q1.txt"));
        assert!(globs.matches("rubric.md"));
        assert!(globs.matches("grader/the_rubric.txt"));
        assert!(globs.matches("a/expected/out.txt"));
        assert!(!globs.matches("src/main.rs"));
        assert!(!globs.matches("answers/q1.txt"));
    }
}
