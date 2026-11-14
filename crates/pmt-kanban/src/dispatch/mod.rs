//! The dispatcher (spec: kanban, Single dispatcher per home; Dispatcher
//! tick; Concurrency caps; Claim allowlist fails closed; Failure circuit
//! breaker; Worker liveness and heartbeats). One tick, under the lease of
//! every live board: reclaim dead, expired and lingering workers, run the
//! promotion pass and wake-ups, then claim and start spawnable tasks in
//! priority then age order, and record a summary event per board.

mod reclaim;

use std::collections::BTreeMap;

use pmt_core::error::Error;
use serde::Serialize;

use crate::board::Board;
use crate::board::create::Profiles;
use crate::board::flow::{ClaimOutcome, Role};
use crate::board::runs::{Breaker, RunEnd};
use crate::boards::Boards;
use crate::lease::Holder;
use crate::model::{Actor, BlockKind, Status, Task};
use crate::ticket::{self, Ticket};

/// Seconds a worker may outlive its closed run before it is terminated.
pub const LINGER_MS: i64 = 120_000;
/// How often workers heartbeat.
pub const HEARTBEAT_MS: i64 = 60_000;

/// The dispatcher's settings (from the dispatching profile's
/// `[kanban]`).
#[derive(Debug, Clone)]
pub struct Settings {
    /// Workers at once across the home.
    pub max_running: u32,
    /// Workers at once per assignee.
    pub max_running_per_profile: u32,
    /// The claim allowlist (`None`: any existing profile).
    pub dispatch_profiles: Option<Vec<String>>,
    /// The assignee of unassigned ready tasks.
    pub default_assignee: Option<String>,
    /// Failure limit and rate-limit cooldown.
    pub breaker: Breaker,
    /// Claim lease length.
    pub claim_ttl_ms: i64,
    /// Route `review` tasks to reviewers.
    pub review_dispatch: bool,
}

/// `kanban.max_running`'s default: physical memory / 512 MiB clamped to
/// 2-8, or 2 when the memory is unknown.
#[must_use]
pub fn default_max_running() -> u32 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let total = system.total_memory();
    if total == 0 {
        return 2;
    }
    u32::try_from(total / (512 * 1024 * 1024))
        .unwrap_or(u32::MAX)
        .clamp(2, 8)
}

/// What the dispatcher asks the binary to start.
#[derive(Debug, Clone)]
pub struct SpawnRequest<'a> {
    /// The board.
    pub board: &'a str,
    /// The claimed task.
    pub task: &'a Task,
    /// The new run.
    pub run_id: &'a str,
    /// The profile it runs as.
    pub profile: &'a str,
    /// Worker or reviewer.
    pub role: Role,
    /// The ticket to write to the worker's stdin.
    pub ticket: Ticket,
}

/// Starts workers; the binary spawns `prometheus kanban work --run`.
pub trait Spawner {
    /// Starts the worker of `request`; returns its pid and OS start time.
    ///
    /// # Errors
    ///
    /// Why it could not start (recorded as a `spawn_failed` run).
    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<(u32, u64), String>;
}

/// One board's tick summary (the `tick` event's payload).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The board.
    pub board: String,
    /// Tasks claimed and started.
    pub claimed: Vec<String>,
    /// Ready tasks with no assignee and no default.
    pub skipped_unassigned: Vec<String>,
    /// Ready tasks whose assignee is outside `kanban.dispatch_profiles`.
    pub skipped_allowlist: Vec<String>,
    /// Ready tasks deferred by a concurrency cap.
    pub capped: Vec<String>,
    /// Runs reclaimed (dead, expired) by task.
    pub reclaimed: Vec<String>,
    /// Tasks blocked by the dispatcher (missing assignee, breaker).
    pub blocked: Vec<String>,
    /// Tasks promoted or woken to `ready`.
    pub promoted: Vec<String>,
}

/// One dispatcher.
pub struct Dispatcher<'a> {
    /// The home's boards.
    pub boards: &'a Boards,
    /// This process.
    pub holder: Holder,
    /// Its settings.
    pub settings: Settings,
    /// Which profiles exist.
    pub profiles: &'a dyn Profiles,
    /// How workers start.
    pub spawner: &'a dyn Spawner,
}

/// Open runs across every board, in total and per profile.
struct Load {
    total: u32,
    per_profile: BTreeMap<String, u32>,
}

impl Load {
    fn add(&mut self, profile: &str) {
        self.total += 1;
        *self.per_profile.entry(profile.to_owned()).or_default() += 1;
    }
}

impl Dispatcher<'_> {
    /// Takes every live board's lease, then ticks each board.
    ///
    /// # Errors
    ///
    /// `kanban.lease_held` (nothing is claimed anywhere), store errors.
    pub fn tick(&self, now: i64) -> Result<Vec<Summary>, Error> {
        let mut boards = Vec::new();
        for slug in self.boards.live()? {
            let mut board = self.boards.board(&slug)?;
            board.acquire_lease(&self.holder, now)?;
            boards.push(board);
        }
        let mut summaries = Vec::new();
        for board in &mut boards {
            let mut summary = Summary {
                board: board.slug().to_owned(),
                ..Summary::default()
            };
            summary.reclaimed = reclaim::reclaim(board, self.settings.breaker, now)?;
            summary.promoted = board.promote_all(&Actor::dispatcher(), now)?;
            summaries.push(summary);
        }
        let mut load = Self::load(&boards)?;
        for (board, summary) in boards.iter_mut().zip(summaries.iter_mut()) {
            self.claim_board(board, summary, &mut load, now)?;
            let payload = serde_json::to_value(&*summary).unwrap_or_default();
            board.record(None, None, &Actor::dispatcher(), "tick", &payload, now)?;
        }
        Ok(summaries)
    }

    /// Gives every lease this process holds back (shutdown).
    pub fn release(&self) {
        if let Ok(slugs) = self.boards.live() {
            for slug in slugs {
                if let Ok(mut board) = self.boards.board(&slug) {
                    let _ = board.release_lease(&self.holder);
                }
            }
        }
    }

    fn load(boards: &[Board]) -> Result<Load, Error> {
        let mut load = Load {
            total: 0,
            per_profile: BTreeMap::new(),
        };
        for board in boards {
            for run in board.open_runs()? {
                load.add(&run.profile);
            }
        }
        Ok(load)
    }

    fn capped(&self, load: &Load, profile: &str, reserve: u32) -> bool {
        load.total + reserve >= self.settings.max_running
            || load.per_profile.get(profile).copied().unwrap_or(0)
                >= self.settings.max_running_per_profile
    }

    fn claim_board(
        &self,
        board: &mut Board,
        summary: &mut Summary,
        load: &mut Load,
        now: i64,
    ) -> Result<(), Error> {
        let ready: Vec<Task> = board
            .tasks(&crate::board::Filter {
                status: Some(Status::Ready),
                ..Default::default()
            })?
            .into_iter()
            .filter(|t| t.not_before.is_none_or(|nb| nb <= now))
            .collect();
        let review = if self.settings.review_dispatch {
            Self::review_candidates(board)?
        } else {
            Vec::new()
        };
        let reserve = u32::from(!review.is_empty() && !ready.is_empty());
        for task in &ready {
            self.offer(board, task, Role::Worker, reserve, summary, load, now)?;
        }
        for task in &review {
            self.offer(board, task, Role::Reviewer, 0, summary, load, now)?;
        }
        Ok(())
    }

    fn review_candidates(board: &Board) -> Result<Vec<Task>, Error> {
        let mut out = Vec::new();
        for task in board.tasks(&crate::board::Filter {
            status: Some(Status::Review),
            ..Default::default()
        })? {
            let idle = board.runs(&task.id)?.iter().all(|r| !r.is_open());
            if task.reviewer.is_some() && idle {
                out.push(task);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)] // one claim's whole context
    fn offer(
        &self,
        board: &mut Board,
        task: &Task,
        role: Role,
        reserve: u32,
        summary: &mut Summary,
        load: &mut Load,
        now: i64,
    ) -> Result<(), Error> {
        let wanted = match role {
            Role::Worker => task
                .assignee
                .clone()
                .or_else(|| self.settings.default_assignee.clone()),
            Role::Reviewer => task.reviewer.clone(),
        };
        let Some(profile) = wanted else {
            summary.skipped_unassigned.push(task.id.clone());
            return Ok(());
        };
        if !self.profiles.exists(&profile) {
            if role == Role::Worker {
                let reason = format!("assignee profile '{profile}' does not exist");
                board.give_up(&task.id, BlockKind::Capability, &reason, now)?;
                summary.blocked.push(task.id.clone());
            }
            return Ok(());
        }
        if self
            .settings
            .dispatch_profiles
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&profile))
        {
            summary.skipped_allowlist.push(task.id.clone());
            return Ok(());
        }
        if self.capped(load, &profile, reserve) {
            summary.capped.push(task.id.clone());
            return Ok(());
        }
        let outcome = match board.claim(
            &Actor::dispatcher(),
            &task.id,
            &profile,
            role,
            self.settings.claim_ttl_ms,
            now,
        ) {
            Ok(outcome) => outcome,
            // Another claimer (a person's manual claim) won the race.
            Err(err) if err.code() == &pmt_core::error::codes::KANBAN_ILLEGAL_TRANSITION => {
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let ClaimOutcome::Claimed(claim) = outcome else {
            return Ok(());
        };
        let request = SpawnRequest {
            board: board.slug(),
            task: &claim.task,
            run_id: &claim.run_id,
            profile: &profile,
            role,
            ticket: Ticket {
                v: ticket::VERSION,
                board: board.slug().to_owned(),
                run: claim.run_id.clone(),
                secret: claim.secret.clone(),
            },
        };
        match self.spawner.spawn(&request) {
            Ok((pid, start)) => {
                board.spawned(&claim.run_id, pid, start, now)?;
                load.add(&profile);
                summary.claimed.push(task.id.clone());
            }
            Err(message) => {
                let end = RunEnd {
                    outcome: "spawn_failed",
                    exit_code: None,
                    message,
                    counts: true,
                };
                if board.end_run(&claim.run_id, &end, self.settings.breaker, now)?
                    == Some(Status::Blocked)
                {
                    summary.blocked.push(task.id.clone());
                }
            }
        }
        Ok(())
    }
}
