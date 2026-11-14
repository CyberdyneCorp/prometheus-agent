//! The desktop's git review pane (spec: desktop-app, Git review pane):
//! the branch, its distance from upstream and the changed files with
//! their diffs in one of three scopes. `uncommitted` compares `HEAD` with
//! the working tree (untracked files included), `branch` the commits since
//! the branch left its base, and `last-turn` the working tree with the
//! snapshot taken when the most recent turn in the repository began, so
//! commits made during that turn are part of it. Only `uncommitted` may
//! change anything.
//!
//! A snapshot is a tree object written through a temporary index (a copy
//! of the real one, so unchanged files are not hashed again): nothing in
//! the working tree, the real index or the refs changes.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::git::{Git, git_error, split_nul};
use crate::repo::Repo;

/// The most of one file's diff shown.
pub const MAX_PATCH: usize = 256 * 1024;

/// A review scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `HEAD` against the working tree.
    Uncommitted,
    /// The branch's commits since its base.
    Branch,
    /// Everything since the last turn in this repository began.
    LastTurn,
}

impl Scope {
    /// The scope named `name` (`uncommitted`, `branch`, `last-turn`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "uncommitted" => Some(Self::Uncommitted),
            "branch" => Some(Self::Branch),
            "last-turn" => Some(Self::LastTurn),
            _ => None,
        }
    }
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Changed {
    /// Its path in the repository.
    pub path: String,
    /// `added`, `modified`, `deleted` or `type-changed`.
    pub status: String,
    /// Part of the change is staged (`uncommitted` only).
    pub staged: bool,
    /// The unified diff (cut at [`MAX_PATCH`]).
    pub patch: String,
}

/// What the pane shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Review {
    /// The working tree's top folder.
    pub root: PathBuf,
    /// The checked-out branch (`None`: detached).
    pub branch: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// The changed files, by path.
    pub files: Vec<Changed>,
    /// Nothing in this scope may be staged, reverted or committed.
    pub read_only: bool,
}

/// A working tree the pane reviews.
#[derive(Debug, Clone)]
pub struct Tree {
    git: Git,
    root: PathBuf,
}

impl Tree {
    /// The working tree containing `dir` (a linked worktree is its own
    /// tree).
    ///
    /// # Errors
    ///
    /// `worktree.not_a_repository` outside a git checkout.
    pub fn at(dir: &Path) -> Result<Self, Error> {
        let repo = Repo::discover(dir)?;
        let top = Git::at(dir, Path::new("")).run(&["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(top);
        Ok(Self {
            git: repo.git_in(&root)?,
            root,
        })
    }

    /// The working tree's top folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A tree object of the working tree as it is now (untracked files
    /// included, ignored ones not).
    ///
    /// # Errors
    ///
    /// `worktree.git`.
    pub fn snapshot(&self) -> Result<String, Error> {
        // One index per snapshot: two snapshots of the tree may run at once.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let real = PathBuf::from(self.git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ])?);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = real.with_file_name(format!(
            "prometheus-review-{}-{n}.index",
            std::process::id()
        ));
        if real.is_file() {
            std::fs::copy(&real, &temp).map_err(|e| git_error(&["snapshot"], &e.to_string()))?;
        }
        let git = self.git.with_index(&temp);
        let tree = git
            .run(&["add", "-A", "--", "."])
            .and_then(|_| git.run(&["write-tree"]));
        let _ = std::fs::remove_file(&temp);
        tree
    }

    /// The review of `scope`; `last_turn` is the snapshot taken when the
    /// last turn in this repository began.
    ///
    /// # Errors
    ///
    /// `worktree.git`; `worktree.no_turn` for `last-turn` before any turn.
    pub fn review(&self, scope: Scope, last_turn: Option<&str>) -> Result<Review, Error> {
        let head = self.head_tree()?;
        let (from, to) = match scope {
            Scope::Uncommitted => (head, self.snapshot()?),
            Scope::Branch => (self.branch_base()?, head),
            Scope::LastTurn => {
                let start = last_turn.ok_or_else(|| {
                    Error::new(
                        &codes::WORKTREE_GIT,
                        "no turn has started in this repository since the daemon started",
                    )
                })?;
                (start.to_owned(), self.snapshot()?)
            }
        };
        let staged = if scope == Scope::Uncommitted {
            self.staged_paths()
        } else {
            Vec::new()
        };
        let mut files = Vec::new();
        for (status, path) in self.name_status(&from, &to)? {
            files.push(Changed {
                patch: self.patch(&from, &to, &path)?,
                staged: staged.contains(&path),
                status,
                path,
            });
        }
        let (ahead, behind) = self.distance();
        Ok(Review {
            root: self.root.clone(),
            branch: self
                .git
                .run(&["symbolic-ref", "--short", "-q", "HEAD"])
                .ok(),
            ahead,
            behind,
            files,
            read_only: scope != Scope::Uncommitted,
        })
    }

    fn empty_tree(&self) -> Result<String, Error> {
        let out = self
            .git
            .run_bytes(&["hash-object", "-t", "tree", "--stdin"], Some(b""))?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    fn head_tree(&self) -> Result<String, Error> {
        match self
            .git
            .run(&["rev-parse", "-q", "--verify", "HEAD^{tree}"])
        {
            Ok(tree) => Ok(tree),
            // An unborn branch: everything is new.
            Err(_) => self.empty_tree(),
        }
    }

    /// Where the branch left its upstream (or the default branch).
    fn branch_base(&self) -> Result<String, Error> {
        for base in ["@{upstream}", "origin/HEAD", "main", "master"] {
            if let Ok(commit) = self.git.run(&["merge-base", "HEAD", base]) {
                return self.git.run(&["rev-parse", &format!("{commit}^{{tree}}")]);
            }
        }
        self.empty_tree()
    }

    fn staged_paths(&self) -> Vec<String> {
        let out = self.git.run_bytes(
            &["diff", "--cached", "--name-only", "-z", "--no-renames"],
            None,
        );
        // An unborn branch has nothing to compare the index with.
        out.map_or_else(|_| self.index_paths(), |bytes| split_nul(&bytes))
    }

    fn index_paths(&self) -> Vec<String> {
        self.git
            .run_bytes(&["ls-files", "-z"], None)
            .map(|bytes| split_nul(&bytes))
            .unwrap_or_default()
    }

    fn name_status(&self, from: &str, to: &str) -> Result<Vec<(String, String)>, Error> {
        let out = self.git.run_bytes(
            &["diff", "--name-status", "-z", "--no-renames", from, to],
            None,
        )?;
        let fields = split_nul(&out);
        Ok(fields
            .chunks(2)
            .filter_map(|pair| match pair {
                [code, path] => Some((status_name(code), path.clone())),
                _ => None,
            })
            .collect())
    }

    fn patch(&self, from: &str, to: &str, path: &str) -> Result<String, Error> {
        let out = self.git.run_bytes(
            &["diff", "--no-color", "--no-renames", from, to, "--", path],
            None,
        )?;
        Ok(cut(String::from_utf8_lossy(&out).into_owned()))
    }

    /// Commits ahead of and behind upstream (none without one).
    fn distance(&self) -> (u32, u32) {
        let Ok(text) = self
            .git
            .run(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        else {
            return (0, 0);
        };
        let mut parts = text.split_whitespace().map(|n| n.parse().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    }

    /// Stages `paths` (new, changed and deleted files alike).
    ///
    /// # Errors
    ///
    /// `worktree.git`.
    pub fn stage(&self, paths: &[String]) -> Result<(), Error> {
        self.git.run(&args(&["add", "-A", "--"], paths)).map(drop)
    }

    /// Unstages `paths`, keeping the working tree.
    ///
    /// # Errors
    ///
    /// `worktree.git`.
    pub fn unstage(&self, paths: &[String]) -> Result<(), Error> {
        if self.git.test(&["rev-parse", "-q", "--verify", "HEAD"]) {
            self.git
                .run(&args(&["restore", "--staged", "--"], paths))
                .map(drop)
        } else {
            self.git
                .run(&args(
                    &["rm", "-q", "--cached", "--ignore-unmatch", "--"],
                    paths,
                ))
                .map(drop)
        }
    }

    /// Discards every change of `paths`: a file `HEAD` has goes back to
    /// it (staged and working copy), a new file is deleted.
    ///
    /// # Errors
    ///
    /// `worktree.git`; the deletion's I/O failure.
    pub fn revert(&self, paths: &[String]) -> Result<(), Error> {
        for path in paths {
            if self.git.test(&["cat-file", "-e", &format!("HEAD:{path}")]) {
                self.git.run(&[
                    "restore",
                    "--source=HEAD",
                    "--staged",
                    "--worktree",
                    "--",
                    path,
                ])?;
            } else {
                self.git
                    .run(&["rm", "-q", "--cached", "--ignore-unmatch", "--", path])?;
                let file = self.root.join(path);
                if file.exists() {
                    std::fs::remove_file(&file)
                        .map_err(|e| git_error(&["revert"], &e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// Commits what is staged with `message`.
    ///
    /// # Errors
    ///
    /// `worktree.git` (nothing staged, an empty message).
    pub fn commit(&self, message: &str) -> Result<String, Error> {
        if message.trim().is_empty() {
            return Err(git_error(&["commit"], "the commit message is empty"));
        }
        self.git.run(&["commit", "-q", "-m", message])?;
        self.git.run(&["rev-parse", "HEAD"])
    }

    /// The staged diff, for a commit message.
    ///
    /// # Errors
    ///
    /// `worktree.git`.
    pub fn staged_diff(&self) -> Result<String, Error> {
        let out = self
            .git
            .run_bytes(&["diff", "--cached", "--no-color", "--stat", "-p"], None)?;
        Ok(cut(String::from_utf8_lossy(&out).into_owned()))
    }
}

/// `text` cut at [`MAX_PATCH`] on a character boundary.
fn cut(mut text: String) -> String {
    if text.len() > MAX_PATCH {
        let mut at = MAX_PATCH;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push_str("\n… diff cut\n");
    }
    text
}

fn args<'a>(head: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    head.iter()
        .copied()
        .chain(paths.iter().map(String::as_str))
        .collect()
}

fn status_name(code: &str) -> String {
    match code.chars().next() {
        Some('A') => "added",
        Some('D') => "deleted",
        Some('T') => "type-changed",
        _ => "modified",
    }
    .to_owned()
}
