//! The repository a worktree belongs to: its main checkout, its common git
//! directory and the `.worktrees/` directory Prometheus trees live in.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use crate::git::Git;

/// The directory under the main checkout holding Prometheus trees.
pub const TREES_DIR: &str = ".worktrees";

/// A non-bare repository with a main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The main checkout's top directory.
    pub root: PathBuf,
    /// The common git directory (`<root>/.git` for an ordinary clone).
    pub common: PathBuf,
}

impl Repo {
    /// The repository containing `dir`; from inside a linked worktree it
    /// is that tree's main repository.
    ///
    /// # Errors
    ///
    /// `worktree.not_a_repository` outside a git checkout, in a bare
    /// repository or without `git`.
    pub fn discover(dir: &Path) -> Result<Self, Error> {
        let not_a_repo = || {
            Error::new(
                &codes::WORKTREE_NOT_A_REPOSITORY,
                format!("{} is not inside a git repository", dir.display()),
            )
        };
        // No hooks run for `rev-parse`; the plain runner suffices here.
        let git = Git::at(dir, Path::new(""));
        let out = git
            .output(&[
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--is-inside-work-tree",
            ])
            .map_err(|_| not_a_repo())?;
        if !out.success {
            return Err(not_a_repo());
        }
        let text = out.text();
        let mut lines = text.lines();
        let common = PathBuf::from(lines.next().ok_or_else(not_a_repo)?);
        if lines.next() != Some("true") {
            return Err(not_a_repo());
        }
        let common = common.canonicalize().unwrap_or(common);
        // A main checkout keeps its git directory at `<root>/.git`; other
        // layouts (`--separate-git-dir`, bare) have no root to host trees.
        if common.file_name().is_none_or(|n| n != ".git") {
            return Err(not_a_repo());
        }
        let root = common.parent().ok_or_else(not_a_repo)?.to_path_buf();
        Ok(Self { root, common })
    }

    /// Whether `dir` still holds this repository.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.common.join("HEAD").is_file()
    }

    /// `<root>/.worktrees`.
    #[must_use]
    pub fn trees_dir(&self) -> PathBuf {
        self.root.join(TREES_DIR)
    }

    /// The empty directory `core.hooksPath` points at.
    #[must_use]
    pub fn no_hooks_dir(&self) -> PathBuf {
        self.common.join("prometheus").join("no-hooks")
    }

    /// A hardened runner in the main checkout.
    ///
    /// # Errors
    ///
    /// `worktree.git` when the empty hooks directory cannot be created.
    pub fn git(&self) -> Result<Git, Error> {
        self.git_in(&self.root)
    }

    /// A hardened runner in `dir` (a tree of this repository).
    ///
    /// # Errors
    ///
    /// As [`Repo::git`].
    pub fn git_in(&self, dir: &Path) -> Result<Git, Error> {
        let hooks = self.no_hooks_dir();
        std::fs::create_dir_all(&hooks).map_err(|e| {
            crate::git::git_error(
                &["hooks"],
                &format!("cannot create {}: {e}", hooks.display()),
            )
        })?;
        Ok(Git::at(dir, &hooks))
    }

    /// Adds `.worktrees/` to the repository's `info/exclude` once.
    ///
    /// # Errors
    ///
    /// `worktree.git` when the file cannot be written.
    pub fn exclude_trees(&self) -> Result<(), Error> {
        let path = self.common.join("info").join("exclude");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let line = format!("/{TREES_DIR}/");
        if current
            .lines()
            .any(|l| l.trim() == line || l.trim() == "/.worktrees" || l.trim() == ".worktrees/")
        {
            return Ok(());
        }
        let mut text = current;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        let io = |e: std::io::Error| {
            crate::git::git_error(
                &["exclude"],
                &format!("cannot write {}: {e}", path.display()),
            )
        };
        std::fs::create_dir_all(path.parent().unwrap_or(&self.common)).map_err(io)?;
        std::fs::write(&path, text).map_err(io)
    }

    /// The remote fetched before creating a tree: the current branch's
    /// upstream remote, else `origin`, else the first remote.
    #[must_use]
    pub fn default_remote(&self, git: &Git) -> Option<String> {
        if let Ok(upstream) =
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            && let Some((remote, _)) = upstream.split_once('/')
        {
            return Some(remote.to_owned());
        }
        let remotes = git.run(&["remote"]).ok()?;
        let names: Vec<&str> = remotes.lines().collect();
        names
            .iter()
            .find(|r| **r == "origin")
            .or_else(|| names.first())
            .map(|r| (*r).to_owned())
    }

    /// Whether `branch` exists on any remote (a remote-tracking ref).
    #[must_use]
    pub fn branch_on_remote(&self, git: &Git, branch: &str) -> bool {
        git.run(&["for-each-ref", "--format=%(refname)", "refs/remotes/"])
            .is_ok_and(|refs| {
                refs.lines().any(|r| {
                    r.split_once('/').is_some()
                        && r.ends_with(&format!("/{branch}"))
                        && r.split('/').count() == 3 + branch.split('/').count()
                })
            })
    }
}
