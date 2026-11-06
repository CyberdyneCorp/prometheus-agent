//! The stale-tree sweep (spec: git-worktrees, Sweep decision table;
//! Archive format).
//!
//! A tree is never touched while a Prometheus session owns it, while any
//! process works inside it, before it has been idle for its tier (24 h
//! for generated names, 72 h for user names) or when it belongs to
//! kanban. Past its tier a clean tree whose commits are pushed or merged
//! is removed; a dirty or unpushed one is archived first (or kept when
//! `archive_dirty` is off); work an archive cannot capture keeps the tree
//! with the reason recorded.

use pmt_core::error::Error;

use crate::archive;
use crate::inspect::{self, State};
use crate::names::Kind;
use crate::repo::Repo;
use crate::tree::{self, Tree};

/// `worktrees.*` as the sweep reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Archive dirty or unpushed trees past their tier (else keep them).
    pub archive_dirty: bool,
    /// Untracked files larger than this keep the tree.
    pub archive_max_file_bytes: u64,
    /// Above this many bytes of trees the oldest idle ones go first (0:
    /// off).
    pub max_total_bytes: u64,
    /// Archives older than this are deleted (0: kept forever).
    pub archive_retention_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            archive_dirty: true,
            archive_max_file_bytes: 52_428_800,
            max_total_bytes: 0,
            archive_retention_secs: 2_592_000,
        }
    }
}

/// What the sweep does to one tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Remove it (nothing to lose).
    Remove,
    /// Archive it, then remove it.
    ArchiveRemove,
    /// Leave it, for the reason given.
    Keep(&'static str),
}

impl Action {
    /// `remove`, `archive` or `keep`, with the reason.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Remove => "remove".to_owned(),
            Self::ArchiveRemove => "archive".to_owned(),
            Self::Keep(reason) => format!("keep ({reason})"),
        }
    }
}

/// One tree's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The tree.
    pub tree: Tree,
    /// What the plan says.
    pub action: Action,
    /// What happened when the plan was applied (`removed`, `archived to
    /// <ref> and removed`, `kept: ...`); the plan itself on a dry run.
    pub outcome: String,
    /// Seconds since it was last active.
    pub idle_secs: u64,
}

/// What a tree looks like to the decision table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Its kind (tier).
    pub kind: Kind,
    /// A live session owns it.
    pub owned: bool,
    /// A live process works inside it.
    pub entered: bool,
    /// Seconds since it was last active.
    pub idle_secs: u64,
    /// Its state; `None` when it could not be read.
    pub state: Option<State>,
}

/// The decision table.
#[must_use]
pub fn decide(facts: &Facts, settings: &Settings) -> Action {
    if facts.kind == Kind::Kanban {
        return Action::Keep("kanban");
    }
    if facts.owned {
        return Action::Keep("live-owner");
    }
    if facts.entered {
        return Action::Keep("live-process");
    }
    if facts.idle_secs < facts.kind.tier_secs() {
        return Action::Keep("recent-activity");
    }
    let Some(state) = &facts.state else {
        return Action::Keep("inspection-failed");
    };
    if !state.submodules.is_empty() {
        return Action::Keep("keep-submodule-work");
    }
    if !state.embedded_repos.is_empty() {
        return Action::Keep("keep-unarchivable");
    }
    if state.worktree_ref_commits > 0 {
        return Action::Keep("keep-worktree-refs");
    }
    if !state.oversized(settings.archive_max_file_bytes).is_empty() {
        return Action::Keep("keep-large-untracked");
    }
    if state.disposable() {
        Action::Remove
    } else if settings.archive_dirty {
        Action::ArchiveRemove
    } else {
        Action::Keep("keep-dirty")
    }
}

/// Whether to change anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Decide and report only.
    DryRun,
    /// Carry the decisions out.
    Apply,
}

/// Sweeps `repo`'s trees at time `now` (Unix seconds).
///
/// # Errors
///
/// `worktree.git` when the repository cannot be read; a failure on one
/// tree keeps that tree and is reported in its outcome.
pub fn sweep(
    repo: &Repo,
    settings: &Settings,
    now: u64,
    mode: Mode,
) -> Result<Vec<Decision>, Error> {
    let cwds = inspect::process_cwds();
    let mut decisions: Vec<Decision> = tree::list(repo)
        .into_iter()
        .map(|tree| plan(repo, tree, settings, now, &cwds))
        .collect();
    apply_budget(&mut decisions, settings);
    if mode == Mode::Apply {
        for decision in &mut decisions {
            decision.outcome = carry_out(repo, &decision.tree, &decision.action, settings);
            let recorded = decision.outcome.clone();
            if decision.tree.exists() {
                decision.tree.record_decision(&recorded);
            }
        }
        expire_archives(repo, settings, now);
    }
    Ok(decisions)
}

fn plan(
    repo: &Repo,
    tree: Tree,
    settings: &Settings,
    now: u64,
    cwds: &[std::path::PathBuf],
) -> Decision {
    let idle_secs = now.saturating_sub(inspect::last_activity(&tree));
    let facts = Facts {
        kind: tree.meta.kind,
        owned: tree::live_owner(&tree).is_some(),
        entered: inspect::entered(cwds, &tree.path),
        idle_secs,
        state: inspect::inspect(repo, &tree).ok(),
    };
    let action = if tree.exists() {
        decide(&facts, settings)
    } else {
        // Its directory is gone: only the bookkeeping is left to prune.
        Action::Keep("missing")
    };
    Decision {
        outcome: action.describe(),
        tree,
        action,
        idle_secs,
    }
}

/// Over `max_total_bytes`, trees kept only because archiving dirty trees
/// is off are archived and removed, oldest idle first.
fn apply_budget(decisions: &mut [Decision], settings: &Settings) {
    if settings.max_total_bytes == 0 {
        return;
    }
    let mut total: u64 = decisions
        .iter()
        .filter(|d| matches!(d.action, Action::Keep(_)))
        .map(|d| inspect::size(&d.tree.path))
        .sum();
    let mut candidates: Vec<usize> = (0..decisions.len())
        .filter(|&i| decisions[i].action == Action::Keep("keep-dirty"))
        .collect();
    candidates.sort_by_key(|&i| std::cmp::Reverse(decisions[i].idle_secs));
    for i in candidates {
        if total <= settings.max_total_bytes {
            break;
        }
        total = total.saturating_sub(inspect::size(&decisions[i].tree.path));
        decisions[i].action = Action::ArchiveRemove;
        decisions[i].outcome = Action::ArchiveRemove.describe();
    }
}

fn carry_out(repo: &Repo, tree: &Tree, action: &Action, settings: &Settings) -> String {
    let result = match action {
        // Its directory is gone: forget only this tree's bookkeeping (its
        // branch stays); other tools' trees are never pruned here.
        Action::Keep("missing") => {
            return match std::fs::remove_dir_all(&tree.admin) {
                Ok(()) => "forgotten: its directory is gone (branch kept)".to_owned(),
                Err(err) => format!("kept: missing ({err})"),
            };
        }
        Action::Keep(reason) => return format!("kept: {reason}"),
        Action::Remove => remove(repo, tree, false).map(|()| "removed".to_owned()),
        Action::ArchiveRemove => {
            archive_then_remove(repo, tree, "sweep", settings, false, &mut || {})
        }
    };
    result.unwrap_or_else(|err| format!("kept: {}", err.message()))
}

/// Archives `tree` and removes it unless it changed meanwhile, someone
/// entered it or archiving failed (it is kept then); its branch is kept
/// with `keep_branch` (a subagent's, for the parent to merge), else as by
/// [`remove`]. `between` runs between building the archive and checking
/// the tree again.
///
/// # Errors
///
/// `worktree.git` when the archive could not be built (the tree is kept).
pub fn archive_then_remove(
    repo: &Repo,
    tree: &Tree,
    reason: &str,
    settings: &Settings,
    keep_branch: bool,
    between: &mut dyn FnMut(),
) -> Result<String, Error> {
    let archived =
        archive::archive_with(repo, tree, reason, settings.archive_max_file_bytes, between)?;
    let reference = archived.archive.reference;
    if archived.changed {
        return Ok(format!(
            "archived to {reference}; kept: changed during archiving"
        ));
    }
    if inspect::entered(&inspect::process_cwds(), &tree.path) || tree::live_owner(tree).is_some() {
        return Ok(format!(
            "archived to {reference}; kept: entered during archiving"
        ));
    }
    remove(repo, tree, keep_branch)?;
    Ok(format!("archived to {reference} and removed"))
}

/// Removes `tree`'s checkout; its branch goes too unless it exists on a
/// remote or `keep_branch` is set.
///
/// # Errors
///
/// `worktree.git`.
pub fn remove(repo: &Repo, tree: &Tree, keep_branch: bool) -> Result<(), Error> {
    let git = repo.git()?;
    let branch = tree.branch();
    let path = tree.path.to_string_lossy().into_owned();
    git.run(&["worktree", "remove", "--force", "--force", &path])?;
    if let Some(branch) = branch
        && !keep_branch
        && !repo.branch_on_remote(&git, &branch)
    {
        git.run(&["branch", "-D", &branch])?;
    }
    Ok(())
}

/// Deletes archives older than the retention.
fn expire_archives(repo: &Repo, settings: &Settings, now: u64) {
    if settings.archive_retention_secs == 0 {
        return;
    }
    let Ok(archives) = archive::list(repo) else {
        return;
    };
    for old in archives
        .iter()
        .filter(|a| now.saturating_sub(a.meta.archived) > settings.archive_retention_secs)
    {
        if let Err(err) = archive::delete(repo, old) {
            tracing::warn!(error = %err, archive = %old.reference, "expired archive not deleted");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(state: State) -> Facts {
        Facts {
            kind: Kind::Generated,
            owned: false,
            entered: false,
            idle_secs: 25 * 3600,
            state: Some(state),
        }
    }

    #[test]
    fn protections_come_before_any_change() {
        let settings = Settings::default();
        let dirty = State {
            dirty: true,
            ..State::default()
        };
        let mut f = facts(dirty.clone());
        f.owned = true;
        assert_eq!(decide(&f, &settings), Action::Keep("live-owner"));
        let mut f = facts(dirty.clone());
        f.entered = true;
        assert_eq!(decide(&f, &settings), Action::Keep("live-process"));
        let mut f = facts(dirty.clone());
        f.idle_secs = 23 * 3600;
        assert_eq!(decide(&f, &settings), Action::Keep("recent-activity"));
        let mut f = facts(dirty);
        f.kind = Kind::Kanban;
        assert_eq!(decide(&f, &settings), Action::Keep("kanban"));
    }

    #[test]
    fn named_trees_wait_three_days() {
        let mut f = facts(State::default());
        f.kind = Kind::Named;
        f.idle_secs = 48 * 3600;
        assert_eq!(
            decide(&f, &Settings::default()),
            Action::Keep("recent-activity")
        );
        f.idle_secs = 73 * 3600;
        assert_eq!(decide(&f, &Settings::default()), Action::Remove);
    }

    #[test]
    fn unarchivable_work_keeps_the_tree_with_its_reason() {
        let settings = Settings::default();
        let cases = [
            (
                State {
                    submodules: vec!["sub".into()],
                    ..State::default()
                },
                "keep-submodule-work",
            ),
            (
                State {
                    embedded_repos: vec!["inner".into()],
                    ..State::default()
                },
                "keep-unarchivable",
            ),
            (
                State {
                    worktree_ref_commits: 1,
                    ..State::default()
                },
                "keep-worktree-refs",
            ),
            (
                State {
                    dirty: true,
                    untracked: vec![("big.bin".into(), 200 * 1024 * 1024)],
                    ..State::default()
                },
                "keep-large-untracked",
            ),
        ];
        for (state, reason) in cases {
            assert_eq!(decide(&facts(state), &settings), Action::Keep(reason));
        }
    }

    #[test]
    fn dirty_trees_are_archived_unless_archiving_is_off() {
        let dirty = State {
            unpushed: 2,
            ..State::default()
        };
        assert_eq!(
            decide(&facts(dirty.clone()), &Settings::default()),
            Action::ArchiveRemove
        );
        let off = Settings {
            archive_dirty: false,
            ..Settings::default()
        };
        assert_eq!(decide(&facts(dirty), &off), Action::Keep("keep-dirty"));
        assert_eq!(decide(&facts(State::default()), &off), Action::Remove);
    }
}
