//! What happens to a session's tree when the session ends (spec:
//! git-worktrees, Exit disposition): a clean tree whose commits are all
//! pushed or merged goes; unfinished work is kept unless the person
//! explicitly chooses otherwise, and nothing ever asks without a person
//! at a terminal.

use pmt_core::error::Error;

use crate::inspect;
use crate::repo::Repo;
use crate::sweep::{self, Settings};
use crate::tree::Tree;

/// The choices offered for a tree with unfinished work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Leave it for the sweep (the default, also on Enter or Esc).
    Keep,
    /// Archive it, then remove it (kept when archiving fails).
    ArchiveRemove,
    /// Remove it and its branch, discarding the work.
    RemoveDiscard,
}

impl Choice {
    /// The choice an answer stands for; anything unrecognized (Enter, Esc,
    /// a typo) keeps the tree.
    #[must_use]
    pub fn parse(answer: &str) -> Self {
        match answer.trim().to_ascii_lowercase().as_str() {
            "a" | "archive" | "archive and remove" => Self::ArchiveRemove,
            "r" | "remove" | "discard" | "remove and discard" => Self::RemoveDiscard,
            _ => Self::Keep,
        }
    }
}

/// What the end of a session means for its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Nothing to lose: remove it.
    Remove,
    /// Unfinished work: ask (interactive) or keep (everything else).
    Unfinished {
        /// Uncommitted changes exist.
        dirty: bool,
        /// Commits nothing else has.
        unpushed: u32,
    },
    /// It could not be inspected: keep it.
    Unknown,
}

/// Looks at `tree` as its session ends.
#[must_use]
pub fn assess(repo: &Repo, tree: &Tree) -> Disposition {
    match inspect::inspect(repo, tree) {
        Ok(state) if state.disposable() => Disposition::Remove,
        Ok(state) => Disposition::Unfinished {
            dirty: state.dirty,
            unpushed: state.unpushed + state.worktree_ref_commits,
        },
        Err(_) => Disposition::Unknown,
    }
}

/// Carries out `choice` for `tree`; returns what to tell the person
/// (including the restore command after an archive).
///
/// # Errors
///
/// `worktree.git` when removal fails (an archive failure keeps the tree
/// and is reported in the text instead).
pub fn apply(
    repo: &Repo,
    tree: &Tree,
    choice: Choice,
    settings: &Settings,
) -> Result<String, Error> {
    match choice {
        Choice::Keep => Ok(format!("worktree kept at {}", tree.path.display())),
        Choice::RemoveDiscard => {
            sweep::remove(repo, tree, false)?;
            Ok(format!("worktree {} removed", tree.path.display()))
        }
        Choice::ArchiveRemove => {
            match sweep::archive_then_remove(repo, tree, "exit", settings, false, &mut || {}) {
                Ok(outcome) => Ok(format!(
                    "{outcome}; restore it with `prometheus worktrees restore {}`",
                    tree.name()
                )),
                Err(err) => Ok(format!(
                    "archiving failed ({}); worktree kept at {}",
                    err.message(),
                    tree.path.display()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Choice;

    #[test]
    fn only_an_explicit_answer_leaves_keep() {
        for keep in ["", "\u{1b}", "k", "keep", "x", "y"] {
            assert_eq!(Choice::parse(keep), Choice::Keep, "{keep:?}");
        }
        assert_eq!(Choice::parse("a"), Choice::ArchiveRemove);
        assert_eq!(Choice::parse("r"), Choice::RemoveDiscard);
    }
}
