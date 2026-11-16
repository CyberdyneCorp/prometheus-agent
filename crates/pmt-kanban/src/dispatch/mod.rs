//! The dispatcher (spec: kanban, Single dispatcher per home; Dispatcher
//! tick; Concurrency caps; Claim allowlist fails closed; Failure circuit
//! breaker; Worker liveness and heartbeats). One tick, under the lease of
//! every live board: reclaim dead, expired and lingering workers, run the
//! promotion pass and wake-ups, then claim and start spawnable tasks in
//! priority then age order, and record a summary event per board.

pub mod guard;
pub mod plan;
mod reclaim;

use std::collections::BTreeMap;

use pmt_core::error::Error;
use serde::Serialize;
use serde_json::json;

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
    /// The respawn guard's windows.
    pub windows: guard::Windows,
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
    /// `profile` names a plugin's worker lane, not a profile.
    pub lane: bool,
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
    /// Ready tasks the respawn guard holds, with the reason.
    pub guarded: Vec<(String, String)>,
    /// Ready tasks whose assignee is neither a profile nor a lane.
    pub skipped_nonspawnable: Vec<String>,
}

impl Summary {
    /// Whether the tick changed nothing on the board.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.claimed.is_empty()
            && self.reclaimed.is_empty()
            && self.blocked.is_empty()
            && self.promoted.is_empty()
    }
}

/// What the dispatcher does with one claimable task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Claim it for this profile (or lane).
    Claim {
        /// The assignee.
        profile: String,
        /// It is a worker lane.
        lane: bool,
    },
    /// No assignee and no default.
    Unassigned,
    /// The assignee profile is gone: block it.
    Missing(String),
    /// The assignee was a lane no enabled plugin declares now.
    Nonspawnable,
    /// Outside `kanban.dispatch_profiles`.
    Allowlist,
    /// A concurrency cap defers it.
    Capped,
    /// The respawn guard holds it.
    Guarded(guard::Guard),
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

/// Records `kind` for `task` unless its latest event already says the
/// same (a held task is not re-logged every tick).
fn note_once(
    board: &mut Board,
    task: &Task,
    kind: &str,
    payload: &serde_json::Value,
    now: i64,
) -> Result<(), Error> {
    let last = board.events(Some(&task.id))?.pop();
    if last.is_some_and(|e| e.kind == kind && &e.payload == payload) {
        return Ok(());
    }
    board.record(
        Some(&task.id),
        None,
        &Actor::dispatcher(),
        kind,
        payload,
        now,
    )?;
    Ok(())
}

/// Open runs across every board, in total and per profile.
pub struct Load {
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

    pub(crate) fn load(boards: &[Board]) -> Result<Load, Error> {
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

    pub(crate) fn review_candidates(board: &Board) -> Result<Vec<Task>, Error> {
        let mut out = Vec::new();
        for task in board.tasks(&crate::board::Filter {
            status: Some(Status::Review),
            ..Default::default()
        })? {
            let idle = board.runs(&task.id)?.iter().all(|r| !r.is_open());
            // An evolve task waits for a person, never a reviewer profile.
            let evolve = matches!(task.kind, crate::model::TaskKind::Evolve { .. });
            if task.reviewer.is_some() && idle && !evolve {
                out.push(task);
            }
        }
        Ok(out)
    }

    /// What to do with `task` for `role` (no board change).
    ///
    /// # Errors
    ///
    /// Store errors (the guard's reads).
    pub fn decide(
        &self,
        board: &Board,
        task: &Task,
        role: Role,
        reserve: u32,
        load: &Load,
        now: i64,
    ) -> Result<Decision, Error> {
        let wanted = match role {
            Role::Worker => task
                .assignee
                .clone()
                .or_else(|| self.settings.default_assignee.clone()),
            Role::Reviewer => task.reviewer.clone(),
        };
        let Some(profile) = wanted else {
            return Ok(Decision::Unassigned);
        };
        let lane = !self.profiles.exists(&profile);
        if lane && !(role == Role::Worker && self.profiles.lane(&profile)) {
            return Ok(match (role, task.lane) {
                (Role::Worker, true) => Decision::Nonspawnable,
                (Role::Worker, false) => Decision::Missing(profile),
                (Role::Reviewer, _) => Decision::Unassigned,
            });
        }
        if self
            .settings
            .dispatch_profiles
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&profile))
        {
            return Ok(Decision::Allowlist);
        }
        if role == Role::Worker
            && let Some(held) = guard::check(board, task, self.settings.windows, now)?
        {
            return Ok(Decision::Guarded(held));
        }
        if self.capped(load, &profile, reserve) {
            return Ok(Decision::Capped);
        }
        Ok(Decision::Claim { profile, lane })
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
        let (profile, lane) = match self.decide(board, task, role, reserve, load, now)? {
            Decision::Claim { profile, lane } => (profile, lane),
            Decision::Unassigned => {
                if role == Role::Worker {
                    summary.skipped_unassigned.push(task.id.clone());
                }
                return Ok(());
            }
            Decision::Missing(profile) => {
                let reason = format!("assignee profile '{profile}' does not exist");
                board.give_up(&task.id, BlockKind::Capability, &reason, now)?;
                summary.blocked.push(task.id.clone());
                return Ok(());
            }
            Decision::Nonspawnable => {
                note_once(
                    board,
                    task,
                    "skipped_nonspawnable",
                    &json!({"assignee": task.assignee}),
                    now,
                )?;
                summary.skipped_nonspawnable.push(task.id.clone());
                return Ok(());
            }
            Decision::Allowlist => {
                summary.skipped_allowlist.push(task.id.clone());
                return Ok(());
            }
            Decision::Capped => {
                summary.capped.push(task.id.clone());
                return Ok(());
            }
            Decision::Guarded(held) => {
                note_once(
                    board,
                    task,
                    "respawn_guarded",
                    &json!({"reason": held.as_str()}),
                    now,
                )?;
                summary
                    .guarded
                    .push((task.id.clone(), held.as_str().to_owned()));
                return Ok(());
            }
        };
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
            lane,
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
