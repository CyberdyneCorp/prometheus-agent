//! `prometheus kanban dispatch --dry-run` (spec: kanban, Respawn guard
//! and dispatch dry run): what a tick would do now, read without taking
//! a lease or changing a row: the tasks it would claim, the tasks the
//! respawn guard holds, and the rate-limited, capped and locked ones.

use pmt_core::error::Error;
use serde::Serialize;

use super::{Decision, Dispatcher};
use crate::board::Filter;
use crate::board::flow::Role;
use crate::model::Status;

/// One board's dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The board.
    pub board: String,
    /// Another live dispatcher holds the board's lease.
    pub locked: Option<String>,
    /// Tasks a tick would claim.
    pub would_claim: Vec<String>,
    /// Tasks the respawn guard holds: (id, reason).
    pub guarded: Vec<(String, String)>,
    /// Ready tasks cooling down after a rate limit: (id, until ms).
    pub rate_limited: Vec<(String, i64)>,
    /// Ready tasks a concurrency cap defers.
    pub capped: Vec<String>,
    /// Ready tasks a tick skips (unassigned, allowlist, not spawnable) or
    /// blocks (assignee gone).
    pub skipped: Vec<String>,
}

impl Dispatcher<'_> {
    /// The dry run of every live board.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn plan(&self, now: i64) -> Result<Vec<Plan>, Error> {
        let mut boards = Vec::new();
        for slug in self.boards.live()? {
            boards.push(self.boards.board(&slug)?);
        }
        let mut load = Self::load(&boards)?;
        let mut plans = Vec::new();
        for board in &boards {
            let mut plan = Plan {
                board: board.slug().to_owned(),
                ..Plan::default()
            };
            if let Some((holder, expires)) = board.lease()?
                && holder != self.holder.label()
                && expires > now
            {
                plan.locked = Some(holder);
            }
            let ready = board.tasks(&Filter {
                status: Some(Status::Ready),
                ..Filter::default()
            })?;
            for task in &ready {
                if let Some(until) = task.not_before.filter(|nb| *nb > now) {
                    plan.rate_limited.push((task.id.clone(), until));
                    continue;
                }
                match self.decide(board, task, Role::Worker, 0, &load, now)? {
                    Decision::Claim { profile, .. } => {
                        load.add(&profile);
                        plan.would_claim.push(task.id.clone());
                    }
                    Decision::Guarded(held) => {
                        plan.guarded
                            .push((task.id.clone(), held.as_str().to_owned()));
                    }
                    Decision::Capped => plan.capped.push(task.id.clone()),
                    _ => plan.skipped.push(task.id.clone()),
                }
            }
            plans.push(plan);
        }
        Ok(plans)
    }
}
