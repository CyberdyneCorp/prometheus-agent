//! The typed workflow table (spec: kanban, Typed workflow table): every
//! status change any surface makes — CLI, tools, dashboard, dispatcher —
//! is checked against [`TABLE`], and the reference page
//! `docs/reference/kanban-workflow.md` is rendered from it by
//! `cargo xtask gen`, so the documentation cannot drift from the rules.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};

use crate::model::{ActorKind, Status};

/// What a transition does, named for the operation that makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    /// A `todo` task whose parents are all settled becomes `ready`.
    Promote,
    /// A `ready` task gains an open parent (link, rejected claim).
    Demote,
    /// The dispatcher (or a person, for an external worker) claims it.
    Claim,
    /// A failed run hands the task back.
    Reclaim,
    /// The failure breaker trips, or the assignee cannot run it.
    GiveUp,
    /// A typed block.
    Block,
    /// A `dependency` block: back to waiting for parents.
    BlockDependency,
    /// The same block cause came back after an unblock.
    BlockLoop,
    /// A block or a schedule is lifted.
    Unblock,
    /// Parked until a time.
    Schedule,
    /// A scheduled task's time came.
    Wake,
    /// Finished (or handed to review by the review lane).
    Complete,
    /// Handed to a reviewer.
    RequestReview,
    /// A reviewer accepts.
    Approve,
    /// A reviewer sends it back.
    RequestChanges,
    /// Put away.
    Archive,
    /// Brought back from the archive.
    Restore,
    /// Moved between the planning columns.
    Plan,
    /// A finished task deliberately queued again.
    Requeue,
    /// A task in review sent back to the queue without a reviewer's
    /// verdict.
    ReopenReview,
    /// A triage idea expanded into a full body.
    Specify,
}

impl Verb {
    /// The name in the reference page.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Demote => "demote",
            Self::Claim => "claim",
            Self::Reclaim => "reclaim",
            Self::GiveUp => "give up",
            Self::Block => "block",
            Self::BlockDependency => "block (dependency)",
            Self::BlockLoop => "block loop",
            Self::Unblock => "unblock",
            Self::Schedule => "schedule",
            Self::Wake => "wake",
            Self::Complete => "complete",
            Self::RequestReview => "request review",
            Self::Approve => "approve",
            Self::RequestChanges => "request changes",
            Self::Archive => "archive",
            Self::Restore => "restore",
            Self::Plan => "plan",
            Self::Requeue => "requeue",
            Self::ReopenReview => "reopen review",
            Self::Specify => "specify",
        }
    }
}

/// A condition the operation checks besides the actor and statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Nothing more.
    None,
    /// Every parent is `done` or `archived`.
    ParentsSettled,
    /// Some parent is open.
    ParentOpen,
    /// Evidence, artifacts, created cards and run ownership (completion
    /// gates).
    CompletionGates,
    /// The run is the task's open run.
    OwnRun,
}

impl Guard {
    const fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::ParentsSettled => "all parents done/archived",
            Self::ParentOpen => "a parent is open",
            Self::CompletionGates => "completion gates",
            Self::OwnRun => "the actor's own open run",
        }
    }
}

/// One row: who may move a task from which statuses to which.
#[derive(Debug, Clone, Copy)]
pub struct Transition {
    /// The operation.
    pub verb: Verb,
    /// Allowed actors.
    pub actors: &'static [ActorKind],
    /// Source statuses.
    pub from: &'static [Status],
    /// Target status.
    pub to: Status,
    /// Extra condition.
    pub guard: Guard,
}

use ActorKind::{Dispatcher as D, Reviewer as R, User as U, Worker as W};
use Status::{Archived, Blocked, Done, Ready, Review, Running, Scheduled, Todo, Triage};

const fn row(
    verb: Verb,
    actors: &'static [ActorKind],
    from: &'static [Status],
    to: Status,
    guard: Guard,
) -> Transition {
    Transition {
        verb,
        actors,
        from,
        to,
        guard,
    }
}

/// Every allowed transition.
pub const TABLE: &[Transition] = &[
    row(
        Verb::Promote,
        &[D, U],
        &[Todo],
        Ready,
        Guard::ParentsSettled,
    ),
    row(Verb::Demote, &[D, U, W], &[Ready], Todo, Guard::ParentOpen),
    row(
        Verb::Claim,
        &[D, U],
        &[Ready],
        Running,
        Guard::ParentsSettled,
    ),
    row(Verb::Reclaim, &[D], &[Running], Ready, Guard::None),
    row(Verb::GiveUp, &[D], &[Running, Ready], Blocked, Guard::None),
    row(Verb::Block, &[W], &[Running], Blocked, Guard::OwnRun),
    row(
        Verb::Block,
        &[U],
        &[Triage, Todo, Ready, Running, Review],
        Blocked,
        Guard::None,
    ),
    row(Verb::BlockDependency, &[W], &[Running], Todo, Guard::OwnRun),
    row(
        Verb::BlockDependency,
        &[U],
        &[Ready, Running],
        Todo,
        Guard::None,
    ),
    row(Verb::BlockLoop, &[W], &[Running], Triage, Guard::OwnRun),
    row(
        Verb::BlockLoop,
        &[U],
        &[Todo, Ready, Running, Review],
        Triage,
        Guard::None,
    ),
    row(
        Verb::Unblock,
        &[U],
        &[Blocked, Scheduled],
        Ready,
        Guard::ParentsSettled,
    ),
    row(
        Verb::Unblock,
        &[U],
        &[Blocked, Scheduled],
        Todo,
        Guard::ParentOpen,
    ),
    row(Verb::Unblock, &[U], &[Blocked], Review, Guard::None),
    row(
        Verb::Schedule,
        &[U],
        &[Todo, Ready, Running, Blocked],
        Scheduled,
        Guard::None,
    ),
    row(Verb::Schedule, &[W], &[Running], Scheduled, Guard::OwnRun),
    row(Verb::Wake, &[D], &[Scheduled], Ready, Guard::ParentsSettled),
    row(Verb::Wake, &[D], &[Scheduled], Todo, Guard::ParentOpen),
    row(
        Verb::Complete,
        &[W],
        &[Running],
        Done,
        Guard::CompletionGates,
    ),
    row(
        Verb::Complete,
        &[W],
        &[Running],
        Review,
        Guard::CompletionGates,
    ),
    row(
        Verb::Complete,
        &[U],
        &[Todo, Ready, Running, Blocked],
        Done,
        Guard::CompletionGates,
    ),
    row(
        Verb::Complete,
        &[U],
        &[Running],
        Review,
        Guard::CompletionGates,
    ),
    row(Verb::RequestReview, &[W], &[Running], Review, Guard::OwnRun),
    row(
        Verb::RequestReview,
        &[U],
        &[Running, Ready],
        Review,
        Guard::None,
    ),
    row(
        Verb::Approve,
        &[R, U],
        &[Review],
        Done,
        Guard::ParentsSettled,
    ),
    row(Verb::RequestChanges, &[R, U], &[Review], Todo, Guard::None),
    row(
        Verb::Archive,
        &[U],
        &[
            Triage, Todo, Scheduled, Ready, Running, Blocked, Review, Done,
        ],
        Archived,
        Guard::None,
    ),
    row(Verb::Restore, &[U], &[Archived], Triage, Guard::None),
    row(Verb::Restore, &[U], &[Archived], Todo, Guard::None),
    row(
        Verb::Restore,
        &[U],
        &[Archived],
        Ready,
        Guard::ParentsSettled,
    ),
    row(Verb::Plan, &[U], &[Triage], Todo, Guard::None),
    row(Verb::Plan, &[U], &[Todo], Triage, Guard::None),
    row(Verb::Requeue, &[U], &[Done], Ready, Guard::ParentsSettled),
    row(
        Verb::ReopenReview,
        &[U],
        &[Review],
        Ready,
        Guard::ParentsSettled,
    ),
    row(Verb::ReopenReview, &[U], &[Review], Todo, Guard::ParentOpen),
    row(Verb::Specify, &[U, D], &[Triage], Todo, Guard::None),
];

/// The rows that move `from` to `to` for `actor`.
pub fn rows(
    actor: ActorKind,
    from: Status,
    to: Status,
) -> impl Iterator<Item = &'static Transition> {
    TABLE
        .iter()
        .filter(move |t| t.to == to && t.from.contains(&from) && t.actors.contains(&actor))
}

/// Checks that the table allows `from → to` for `actor` by `verb`.
///
/// # Errors
///
/// `kanban.illegal_transition`: `illegal transition <from> → <to> for
/// <actor>`.
pub fn check(verb: Verb, actor: ActorKind, from: Status, to: Status) -> Result<(), Error> {
    if rows(actor, from, to).any(|t| t.verb == verb) {
        Ok(())
    } else {
        Err(illegal(actor, from, to))
    }
}

/// The verb a person's plain move (a dashboard drag) from `from` to `to`
/// performs, when the table has one.
///
/// # Errors
///
/// `kanban.illegal_transition`.
pub fn move_verb(actor: ActorKind, from: Status, to: Status) -> Result<Verb, Error> {
    rows(actor, from, to)
        .map(|t| t.verb)
        .next()
        .ok_or_else(|| illegal(actor, from, to))
}

/// The refusal of a transition the table lacks.
#[must_use]
pub fn illegal(actor: ActorKind, from: Status, to: Status) -> Error {
    Error::new(
        &codes::KANBAN_ILLEGAL_TRANSITION,
        format!("illegal transition {from} → {to} for {actor}"),
    )
}

/// The reference page rendered from [`TABLE`].
#[must_use]
pub fn markdown() -> String {
    let mut out = String::from(
        "# Kanban workflow\n\n<!-- Generated by `cargo xtask gen` from \
         pmt_kanban::workflow::TABLE; do not edit. -->\n\n\
         Every status change on a board is one of these transitions. Anything \
         else is refused with `illegal transition <from> → <to> for <actor>` \
         (exit 75; HTTP 409 from the dashboard API).\n\n\
         | Operation | Actors | From | To | Guard |\n|---|---|---|---|---|\n",
    );
    for t in TABLE {
        let actors: Vec<_> = t.actors.iter().map(|a| a.as_str()).collect();
        let from: Vec<_> = t.from.iter().map(|s| format!("`{s}`")).collect();
        let _ = writeln!(
            out,
            "| {} | {} | {} | `{}` | {} |",
            t.verb.as_str(),
            actors.join(", "),
            from.join(", "),
            t.to,
            t.guard.text()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_from_triage_to_running_names_the_actor() {
        let err = move_verb(ActorKind::User, Triage, Running).unwrap_err();
        assert_eq!(
            err.message(),
            "illegal transition triage → running for user"
        );
        assert_eq!(err.exit_code(), 75);
    }

    #[test]
    fn archived_restores_only_to_planning_columns() {
        for to in Status::ALL {
            let allowed = rows(ActorKind::User, Archived, to).any(|t| t.verb == Verb::Restore);
            assert_eq!(allowed, matches!(to, Triage | Todo | Ready), "{to}");
        }
    }

    #[test]
    fn request_changes_lands_in_todo() {
        check(Verb::RequestChanges, ActorKind::Reviewer, Review, Todo).unwrap();
        assert!(check(Verb::RequestChanges, ActorKind::Reviewer, Review, Ready).is_err());
    }
}
