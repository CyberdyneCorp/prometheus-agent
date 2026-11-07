//! Git fixture repositories for the worktree tests: a bare `origin` and a
//! clone `work` on `main` with one pushed commit, under the target's temp
//! directory and with the user's git configuration ignored.

#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A fixture.
pub struct Fixture {
    _root: tempfile::TempDir,
    /// The bare remote.
    pub origin: PathBuf,
    /// The clone people work in.
    pub work: PathBuf,
}

/// Runs git in `dir` without the user's configuration; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    /// A fresh fixture.
    pub fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = root.path().canonicalize().unwrap();
        let origin = base.join("origin.git");
        let work = base.join("work");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "user.email", "t@example.invalid"]);
        std::fs::write(work.join("README.md"), "hello\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(
            &work,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(&work, &["push", "-q", "-u", "origin", "main"]);
        git(&work, &["remote", "set-head", "origin", "main"]);
        Self {
            _root: root,
            origin,
            work,
        }
    }

    /// The profile directory of the fixture (for the registry).
    pub fn profile(&self) -> PathBuf {
        let dir = self.work.parent().unwrap().join("profile");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

/// Commits everything in `dir` with `message`.
pub fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}
