//! Stopping a run on a person's request (spec: web-dashboard, Kanban
//! dashboard API and orchestration modes, "Terminate a stuck run"). The
//! run is closed first, without counting towards the failure breaker, so
//! its task is back in `ready` before the worker dies: the dispatcher
//! never sees the dying worker as a failed run, and a worker that still
//! reports afterwards is refused because its run is no longer open. Then
//! the worker is signalled, identified by pid and start time as the
//! liveness rules require; one that outlives the signal is reaped by the
//! dispatcher like any worker that lingers after its run closed.

use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use serde_json::json;

use super::Board;
use super::runs::{Breaker, RunEnd};
use crate::model::{Actor, Status};
use crate::process::{self, Terminated};
use crate::workflow::{self, Verb};

/// How long a terminated worker is waited for before the reaper is left
/// to finish it.
const EXIT_WAIT: Duration = Duration::from_secs(3);

/// What a termination did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    /// The run's task.
    pub task_id: String,
    /// The task's status afterwards (`ready` for a worker run).
    pub status: Status,
    /// What happened to the worker process (`None`: no process was
    /// recorded, as for a manual claim).
    pub signal: Option<Terminated>,
}

fn ended(id: &str) -> Error {
    Error::new(
        &codes::KANBAN_ILLEGAL_TRANSITION,
        format!("run '{id}' already ended; there is nothing to terminate"),
    )
}

/// Waits until the worker `pid` started at `start` is gone.
fn exited(pid: u32, start: u64) -> bool {
    let deadline = Instant::now() + EXIT_WAIT;
    while process::alive(pid, start) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

impl Board {
    /// Terminates open run `id`: closes it as `reclaimed` (not counted as
    /// a failure), returning a worker run's task to `ready`, records a
    /// `terminated` event by `actor`, then stops its worker process.
    ///
    /// # Errors
    ///
    /// `kanban.run_not_found`, `kanban.illegal_transition` when the run
    /// already ended or the actor may not reclaim its task, store errors.
    pub fn terminate(
        &mut self,
        actor: &Actor,
        id: &str,
        breaker: Breaker,
        now: i64,
    ) -> Result<Stopped, Error> {
        let run = self.run(id)?;
        if !run.is_open() {
            return Err(ended(id));
        }
        let task = self.task(&run.task_id)?;
        if run.role == "worker" {
            workflow::check(Verb::Reclaim, actor.kind, task.status, Status::Ready)?;
        }
        let end = RunEnd {
            outcome: "reclaimed",
            exit_code: None,
            message: format!("terminated by {}", actor.label()),
            counts: false,
        };
        let status = self
            .end_run(id, &end, breaker, now)?
            .ok_or_else(|| ended(id))?;
        let signal = run
            .pid
            .zip(run.pid_start)
            .map(|(pid, start)| (process::terminate(pid, start), pid, start));
        let said = signal.map_or("none", |(s, _, _)| s.as_str());
        self.record(
            Some(&task.id),
            Some(id),
            actor,
            "terminated",
            &json!({"signal": said}),
            now,
        )?;
        match signal {
            Some((Terminated::Signalled, pid, start)) if exited(pid, start) => {
                self.worker_gone(id, true)?;
            }
            Some((Terminated::Gone | Terminated::PidReused, _, _)) => {
                self.worker_gone(id, false)?;
            }
            _ => {}
        }
        Ok(Stopped {
            task_id: task.id,
            status,
            signal: signal.map(|(s, _, _)| s),
        })
    }
}
