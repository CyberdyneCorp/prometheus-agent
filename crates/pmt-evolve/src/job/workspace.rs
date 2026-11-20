//! Rollout workspaces (spec: harness-evolution, Isolated rollouts;
//! Evolution task capture). A rollout runs in a fresh directory built from
//! the frozen task: for a git task, a checkout of the recorded commit
//! from a `--shared` clone (which only reads the user's repository) with
//! every leak-glob path tracked at that commit deleted, re-committed as a
//! self-contained one-commit history (no alternates, no excluded blob),
//! then `uncommitted.patch` applied; otherwise a copy of `task/source/`. The
//! user's own workspace is never touched. A pristine copy taken before the
//! rollout gives the workspace diff it delivers.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

use super::task::RepoSnapshot;

/// Image extensions a rollout may deliver.
const IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

fn failed(message: impl Into<String>) -> Error {
    Error::new(&codes::HARNESS_IO, message)
}

/// Builds the workspace of the task at `task` (`<job>/task`) in `target`
/// (which must not exist).
///
/// # Errors
///
/// `harness.io` when git fails or files cannot be copied.
pub fn materialize(task: &Path, target: &Path) -> Result<(), Error> {
    let source = task.join("source");
    if !source.join("repo.toml").is_file() {
        return private_fs::copy_tree(&source, target).map_err(|e| io(target, &e));
    }
    let repo = RepoSnapshot::read(&source)?;
    let root = repo
        .root
        .as_deref()
        .ok_or_else(|| failed("repo.toml names no repository"))?;
    let parent = target.parent().unwrap_or(target);
    private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
    git(
        parent,
        &[
            "clone",
            "--shared",
            "--no-checkout",
            "--quiet",
            root,
            &target.to_string_lossy(),
        ],
    )?;
    git(
        target,
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--quiet",
            "--detach",
            &repo.commit,
        ],
    )?;
    repo.remove_excluded(target)?;
    fresh_history(target)?;
    let patch = source.join("uncommitted.patch");
    if std::fs::metadata(&patch).is_ok_and(|m| m.len() > 0) {
        let patch = patch.to_string_lossy();
        let mut args = vec!["apply", "--binary"];
        let excludes: Vec<String> = repo
            .excluded
            .iter()
            .map(|p| format!("--exclude={p}"))
            .collect();
        args.extend(excludes.iter().map(String::as_str));
        args.push(&patch);
        git(target, &args)?;
    }
    repo.remove_excluded(target)
}

/// Replaces the clone's history with one commit of the workspace as it is
/// now (the recorded commit without its excluded paths). The clone's own
/// history still holds the excluded files' blobs, reachable from HEAD
/// through the alternates file into the user's object store, so a
/// rollout could otherwise get every answer back with `git show
/// HEAD:<path>`; and under the strict local sandbox those alternates are
/// unreadable, which would break `git status` inside the rollout. The
/// user's hooks, templates and signing settings are kept out.
fn fresh_history(target: &Path) -> Result<(), Error> {
    let dot_git = target.join(".git");
    std::fs::remove_dir_all(&dot_git).map_err(|e| io(&dot_git, &e))?;
    let hooks = format!("core.hooksPath={}", dot_git.join("no-hooks").display());
    git(target, &["init", "--quiet", "--template="])?;
    git(target, &["-c", &hooks, "add", "--all"])?;
    git(
        target,
        &[
            "-c",
            &hooks,
            "-c",
            "user.name=Prometheus",
            "-c",
            "user.email=prometheus@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--no-verify",
            "--allow-empty",
            "-m",
            "task workspace",
        ],
    )
}

fn git(dir: &Path, args: &[&str]) -> Result<(), Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| failed(format!("git could not run: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(failed(format!(
        "git {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Every file under `root` (relative, sorted), skipping `.git`.
fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && entry.file_name() != ".git" => walk(root, &path, out),
                Ok(kind) if kind.is_file() => out.push(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                ),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The paths added or changed from `before` to `after`, and those
/// removed.
fn changes(before: &Path, after: &Path) -> (Vec<String>, Vec<String>) {
    let old = files(before);
    let new = files(after);
    let changed = new
        .iter()
        .filter(|p| std::fs::read(before.join(p)).ok() != std::fs::read(after.join(p)).ok())
        .cloned()
        .collect();
    let removed = old.into_iter().filter(|p| !new.contains(p)).collect();
    (changed, removed)
}

/// The unified diff from `before` to `after` (`.git` ignored; binary
/// files named, not shown).
#[must_use]
pub fn diff(before: &Path, after: &Path) -> String {
    let (changed, removed) = changes(before, after);
    let mut paths: Vec<&String> = changed.iter().chain(&removed).collect();
    paths.sort();
    let mut out = String::new();
    for path in paths {
        let old = std::fs::read(before.join(path)).unwrap_or_default();
        let new = std::fs::read(after.join(path)).unwrap_or_default();
        match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
            (Ok(old), Ok(new)) => out.push_str(
                &similar::TextDiff::from_lines(old, new)
                    .unified_diff()
                    .header(&format!("a/{path}"), &format!("b/{path}"))
                    .to_string(),
            ),
            _ => {
                let _ = writeln!(out, "Binary file {path} differs");
            }
        }
    }
    out
}

/// Images added or changed from `before` to `after` (at most `max`).
#[must_use]
pub fn new_images(before: &Path, after: &Path, max: usize) -> Vec<PathBuf> {
    changes(before, after)
        .0
        .into_iter()
        .filter(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| IMAGES.contains(&e.to_ascii_lowercase().as_str()))
        })
        .take(max)
        .map(|p| after.join(p))
        .collect()
}

/// Copies `workspace` (without `.git`) to `target`: the pristine copy
/// a rollout's diff is taken against.
///
/// # Errors
///
/// I/O errors.
pub fn snapshot(workspace: &Path, target: &Path) -> Result<(), Error> {
    private_fs::create_dir(target).map_err(|e| io(target, &e))?;
    for relative in files(workspace) {
        let to = target.join(&relative);
        if let Some(parent) = to.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        std::fs::copy(workspace.join(&relative), &to).map_err(|e| io(&to, &e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copied_task_diffs_its_changes_and_finds_new_images() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("task");
        std::fs::create_dir_all(task.join("source/sub")).unwrap();
        std::fs::write(task.join("source/a.txt"), "one\n").unwrap();
        std::fs::write(task.join("source/sub/b.txt"), "two\n").unwrap();
        let ws = dir.path().join("ws");
        materialize(&task, &ws).unwrap();
        let before = dir.path().join("before");
        snapshot(&ws, &before).unwrap();
        std::fs::write(ws.join("a.txt"), "uno\n").unwrap();
        std::fs::remove_file(ws.join("sub/b.txt")).unwrap();
        std::fs::write(ws.join("chart.png"), [0x89, b'P', b'N', b'G', 0xff]).unwrap();
        let text = diff(&before, &ws);
        assert!(text.contains("-one\n+uno"), "{text}");
        assert!(text.contains("--- a/sub/b.txt"), "{text}");
        assert!(text.contains("Binary file chart.png differs"), "{text}");
        assert_eq!(new_images(&before, &ws, 8), [ws.join("chart.png")]);
        assert_eq!(
            std::fs::read_to_string(task.join("source/a.txt")).unwrap(),
            "one\n"
        );
    }
}
