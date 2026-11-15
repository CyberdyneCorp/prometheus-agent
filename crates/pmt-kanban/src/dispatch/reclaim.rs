//! Reclaiming runs (spec: kanban, Worker liveness and heartbeats; Failure
//! circuit breaker). A worker is alive only while its pid runs with the
//! start time recorded at spawn: a dead or recycled pid is reclaimed (the
//! recycled one never signalled), an expired claim is reclaimed as a
//! failure, and a worker still alive 120 s after its run closed is
//! terminated with a `worker_reaped` event.

use pmt_core::error::Error;
use serde_json::json;

use super::LINGER_MS;
use crate::board::Board;
use crate::board::runs::{Breaker, RunEnd};
use crate::model::{Actor, Run};
use crate::process::{self, Terminated};

/// Whether `run`'s worker runs; `None` when no worker was recorded.
fn alive(run: &Run) -> Option<bool> {
    let (pid, start) = run.pid.zip(run.pid_start)?;
    Some(process::alive(pid, start))
}

/// How an open run ends now, if it does.
fn verdict(board: &Board, run: &Run, now: i64) -> Result<Option<RunEnd>, Error> {
    match alive(run) {
        Some(false) => Ok(Some(match board.recorded_exit(&run.id)? {
            Some((code, message)) => RunEnd::exited(code, message),
            None => RunEnd {
                outcome: "crashed",
                exit_code: None,
                message: "worker died without recording an exit".to_owned(),
                counts: true,
            },
        })),
        _ if run.lease_expires_at.is_some_and(|at| at <= now) => {
            if let (Some(true), Some(pid), Some(start)) = (alive(run), run.pid, run.pid_start) {
                let _ = process::terminate(pid, start);
            }
            Ok(Some(RunEnd {
                outcome: "timed_out",
                exit_code: None,
                message: "claim expired without a worker heartbeat".to_owned(),
                counts: true,
            }))
        }
        _ => Ok(None),
    }
}

/// Reclaims `board`'s dead and expired runs and reaps lingering workers;
/// returns the reclaimed task ids.
pub(super) fn reclaim(board: &mut Board, breaker: Breaker, now: i64) -> Result<Vec<String>, Error> {
    let mut reclaimed = Vec::new();
    for run in board.open_runs()? {
        if let Some(end) = verdict(board, &run, now)? {
            board.end_run(&run.id, &end, breaker, now)?;
            if alive(&run) != Some(true) {
                board.worker_gone(&run.id, false)?;
            }
            reclaimed.push(run.task_id.clone());
        }
    }
    for run in board.lingering()? {
        match alive(&run) {
            Some(true) if run.ended_at.is_some_and(|end| end + LINGER_MS <= now) => {
                let (Some(pid), Some(start)) = (run.pid, run.pid_start) else {
                    continue;
                };
                let result = process::terminate(pid, start);
                board.record(
                    Some(&run.task_id),
                    Some(&run.id),
                    &Actor::dispatcher(),
                    "worker_reaped",
                    &json!({"pid": pid, "signal": result.as_str()}),
                    now,
                )?;
                if result != Terminated::Failed {
                    board.worker_gone(&run.id, true)?;
                }
            }
            Some(true) => {}
            _ => board.worker_gone(&run.id, false)?,
        }
    }
    Ok(reclaimed)
}
