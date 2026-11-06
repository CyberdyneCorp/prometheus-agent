//! What a tree holds (spec: git-worktrees, Sweep decision table; Exit
//! disposition): uncommitted work, commits nothing else has, work an
//! archive cannot capture, and when it was last touched. Every read runs
//! with optional locks off, so inspecting never rewrites the tree's index.

use std::path::{Path, PathBuf};

use pmt_core::error::Error;

use crate::git::{Git, split_nul};
use crate::repo::Repo;
use crate::tree::{Tree, secs_of};

/// A tree's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// Staged, unstaged or untracked (non-ignored) changes exist.
    pub dirty: bool,
    /// Commits on `HEAD` that no other branch and no remote has.
    pub unpushed: u32,
    /// Untracked files, with their sizes.
    pub untracked: Vec<(PathBuf, u64)>,
    /// Untracked directories that are repositories of their own.
    pub embedded_repos: Vec<PathBuf>,
    /// Submodules checked out inside the tree.
    pub submodules: Vec<PathBuf>,
    /// Commits reachable only from per-worktree refs (`refs/worktree/*`,
    /// `refs/bisect/*`).
    pub worktree_ref_commits: u32,
}

impl State {
    /// Nothing to lose: clean and every commit pushed or merged.
    #[must_use]
    pub fn disposable(&self) -> bool {
        !self.dirty && self.unpushed == 0 && self.worktree_ref_commits == 0
    }

    /// The largest untracked file over `limit`, if any.
    #[must_use]
    pub fn oversized(&self, limit: u64) -> Vec<&(PathBuf, u64)> {
        self.untracked
            .iter()
            .filter(|(_, size)| *size > limit)
            .collect()
    }
}

/// Reads the state of `tree`.
///
/// # Errors
///
/// `worktree.git` when git cannot read the tree (a corrupt index, a
/// missing checkout): the inspection probe failed.
pub fn inspect(repo: &Repo, tree: &Tree) -> Result<State, Error> {
    let git = repo.git_in(&tree.path)?;
    let status = git.run_bytes(
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ],
        None,
    )?;
    let mut state = State::default();
    for entry in split_nul(&status) {
        state.dirty = true;
        let Some(path) = entry.get(3..) else { continue };
        if !entry.starts_with("??") {
            continue;
        }
        let full = tree.path.join(path);
        if path.ends_with('/') {
            state
                .embedded_repos
                .push(PathBuf::from(path.trim_end_matches('/')));
        } else {
            let size = std::fs::symlink_metadata(&full).map_or(0, |m| m.len());
            state.untracked.push((PathBuf::from(path), size));
        }
    }
    state.unpushed = unpushed(&git, tree.branch().as_deref())?;
    state.submodules = submodules(&git, &tree.path)?;
    state.worktree_ref_commits = worktree_ref_commits(&git)?;
    Ok(state)
}

/// Commits of `HEAD` not on any remote nor any other local branch.
fn unpushed(git: &Git, branch: Option<&str>) -> Result<u32, Error> {
    let exclude = branch.map(|b| format!("--exclude={b}"));
    let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
    if let Some(exclude) = &exclude {
        args.push(exclude);
    }
    args.push("--branches");
    let count = git.run(&args)?;
    Ok(count.trim().parse().unwrap_or(0))
}

/// Initialized submodules: gitlinks whose directory holds a checkout.
fn submodules(git: &Git, root: &Path) -> Result<Vec<PathBuf>, Error> {
    let staged = git.run_bytes(&["ls-files", "--stage", "-z"], None)?;
    Ok(split_nul(&staged)
        .iter()
        .filter(|line| line.starts_with("160000 "))
        .filter_map(|line| line.split_once('\t').map(|(_, path)| PathBuf::from(path)))
        .filter(|path| root.join(path).join(".git").exists())
        .collect())
}

/// Commits only per-worktree refs reach.
fn worktree_ref_commits(git: &Git) -> Result<u32, Error> {
    let refs = git.run(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/worktree/",
        "refs/bisect/",
        "refs/rewritten/",
    ])?;
    let mut total = 0;
    for name in refs.lines().filter(|l| !l.is_empty()) {
        let count = git.run(&[
            "rev-list",
            "--count",
            name,
            "--not",
            "HEAD",
            "--branches",
            "--remotes",
            "--tags",
        ])?;
        total += count.trim().parse::<u32>().unwrap_or(0);
    }
    Ok(total)
}

/// When the tree was last active: the newest of its files', `HEAD`'s,
/// reflog's and index's modification times (Unix seconds).
#[must_use]
pub fn last_activity(tree: &Tree) -> u64 {
    let mut newest = ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|f| modified(&tree.admin.join(f)))
        .max()
        .unwrap_or(0);
    newest = newest.max(tree.meta.created);
    let mut stack = vec![tree.path.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            newest = newest.max(meta.modified().map_or(0, secs_of));
            // A nested `.git` is another repository's bookkeeping.
            if meta.is_dir() && entry.file_name() != ".git" {
                stack.push(entry.path());
            }
        }
    }
    newest
}

fn modified(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok()?.modified().ok().map(secs_of)
}

/// The total size in bytes of the tree's files.
#[must_use]
pub fn size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// The working directories of every other process.
#[must_use]
pub fn process_cwds() -> Vec<PathBuf> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    let own = std::process::id();
    system
        .processes()
        .iter()
        .filter(|(pid, _)| pid.as_u32() != own)
        .filter_map(|(_, process)| process.cwd().map(Path::to_path_buf))
        .collect()
}

/// Whether one of `cwds` is inside `path`.
#[must_use]
pub fn entered(cwds: &[PathBuf], path: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    cwds.iter().any(|cwd| cwd.starts_with(&path))
}
