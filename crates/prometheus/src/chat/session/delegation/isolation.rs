//! Subagent worktree isolation (spec: git-worktrees, Subagent worktree
//! isolation; subagent-delegation, Worktree isolation for children).
//!
//! With `delegation.worktree_isolation` on, a child of a session on the
//! local backend inside a git repository runs in its own tree
//! `<repo>/.worktrees/subagent-<id>` on branch
//! `<delegation.worktree_branch_prefix>/subagent-<id>`, started at the
//! parent's `HEAD` so the branch merges back cleanly. On finish a tree
//! with nothing in it is removed with its branch once an inspection
//! succeeded; one with commits or edits is archived and removed with its
//! branch kept for the parent to merge (kept in place when archiving is
//! off or fails); a tree that cannot be inspected stays. Elsewhere the
//! child shares the parent's workspace and its result says why.

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_delegation::result::WorktreeReport;
use pmt_worktrees::create::{self, Request};
use pmt_worktrees::sweep::{self, Settings};
use pmt_worktrees::{Kind, Repo, Tree, inspect};

/// Where a child runs.
pub struct Isolation {
    /// Its working directory.
    pub cwd: PathBuf,
    /// Its own tree, when isolated.
    pub tree: Option<Isolated>,
    /// Why it was not isolated although configured to be.
    pub note: Option<String>,
}

/// A child's own tree.
pub struct Isolated {
    repo: Repo,
    tree: Tree,
    /// The commit it started at (its commits are those after it).
    base: String,
    settings: Settings,
}

impl Isolation {
    /// The child runs in `cwd`, shared with its parent.
    fn shared(cwd: &Path, note: Option<String>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            tree: None,
            note,
        }
    }

    /// What the child is told about its tree (appended to its prompt).
    #[must_use]
    pub fn instructions(&self) -> Option<String> {
        let tree = self.tree.as_ref()?;
        Some(format!(
            "\n\nYou work in your own git worktree at {} on branch {}. Make every change \
             there and commit it on that branch before you finish; the parent merges the \
             branch. Do not push.",
            tree.tree.path.display(),
            tree.tree.meta.branch
        ))
    }
}

/// Sets up where child `subagent_id` of a session in `parent_cwd` runs,
/// by the child profile's `scope`.
#[must_use]
pub fn prepare(scope: &ProfileScope, parent_cwd: &Path, subagent_id: &str) -> Isolation {
    let config = scope.config();
    if !config.delegation.worktree_isolation {
        return Isolation::shared(parent_cwd, None);
    }
    let skipped = |why: String| {
        Isolation::shared(
            parent_cwd,
            Some(format!(
                "worktree isolation skipped: {why}; the child ran in the parent's workspace"
            )),
        )
    };
    let backend = &config.terminal.backend;
    if backend != "local" {
        return skipped(format!("the terminal backend is '{backend}', not local"));
    }
    let Ok(repo) = Repo::discover(parent_cwd) else {
        return skipped(format!(
            "{} is not inside a git repository",
            parent_cwd.display()
        ));
    };
    let head = repo
        .git_in(parent_cwd)
        .and_then(|git| git.run(&["rev-parse", "--verify", "HEAD"]));
    let head = match head {
        Ok(head) => head,
        Err(err) => return skipped(err.message().to_owned()),
    };
    let name = format!("subagent-{subagent_id}");
    let request = Request {
        name: Some(&name),
        kind: Kind::Subagent,
        branch_prefix: &config.delegation.worktree_branch_prefix,
        sync_remote: false,
        base: Some(&head),
    };
    match create::create(parent_cwd, &request) {
        Ok(created) => {
            if let Err(err) = pmt_worktrees::registry::record(scope.dir(), &created.repo) {
                tracing::warn!(error = %err, "worktree repository not recorded");
            }
            Isolation {
                cwd: created.tree.path.clone(),
                note: None,
                tree: Some(Isolated {
                    repo: created.repo,
                    tree: created.tree,
                    base: head,
                    settings: settings_of(scope),
                }),
            }
        }
        Err(err) => skipped(err.message().to_owned()),
    }
}

/// The sweep settings of `scope`'s `[worktrees]`.
#[must_use]
pub fn settings_of(scope: &ProfileScope) -> Settings {
    let w = &scope.config().worktrees;
    Settings {
        archive_dirty: w.archive_dirty,
        archive_max_file_bytes: w.archive_max_file_bytes,
        max_total_bytes: w.max_total_bytes,
        archive_retention_secs: w.archive_retention_secs,
    }
}

impl Isolated {
    /// Disposes of the tree as its child finished (blocking git calls).
    #[must_use]
    pub fn finish(&self) -> WorktreeReport {
        let mut report = WorktreeReport {
            path: self.tree.path.to_string_lossy().into_owned(),
            branch: self.tree.meta.branch.clone(),
            commits: self.commits(),
            ..WorktreeReport::default()
        };
        // The inspection probe: nothing is removed unless it succeeds.
        let state = match inspect::inspect(&self.repo, &self.tree) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(error = %err, tree = %report.path, "subagent worktree kept: inspection failed");
                report.inspection_failed = true;
                return report;
            }
        };
        report.dirty = state.dirty;
        if !state.dirty && report.commits.is_empty() && state.worktree_ref_commits == 0 {
            match sweep::remove(&self.repo, &self.tree, false) {
                Ok(()) => report.pruned = true,
                Err(err) => tracing::warn!(error = %err, "clean subagent worktree not removed"),
            }
            return report;
        }
        if !self.settings.archive_dirty {
            return report;
        }
        let archived = sweep::archive_then_remove(
            &self.repo,
            &self.tree,
            "subagent",
            &self.settings,
            true,
            &mut || {},
        );
        match archived {
            Ok(outcome) => {
                report.archive_ref = Some(format!(
                    "{}{}",
                    pmt_worktrees::archive::PREFIX,
                    self.tree.name()
                ));
                tracing::info!(tree = %report.path, "{outcome}");
            }
            Err(err) => {
                tracing::warn!(error = %err, tree = %report.path, "subagent worktree kept: archiving failed");
            }
        }
        report
    }

    /// The child's commits, oldest first (`<short sha> <subject>`).
    fn commits(&self) -> Vec<String> {
        let range = format!("{}..HEAD", self.base);
        self.repo
            .git_in(&self.tree.path)
            .and_then(|git| git.run(&["log", "--reverse", "--format=%h %s", &range]))
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}
