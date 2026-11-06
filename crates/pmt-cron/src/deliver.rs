//! Response framing, silence and the durable delivery outbox (spec:
//! scheduled-jobs, Response framing and silence; Durable delivery outbox;
//! Delivery targets).
//!
//! A run's messaging deliveries are queued in `cron_deliveries` and sent
//! by [`pump`] through a [`Sender`] (the daemon's gateway): each row at
//! most once, transient failures retried up to 3 times with exponential
//! backoff from 30 s (30, 60, 120 s), a row whose sender died mid-send settled `unknown`.
//! When every row of a run is settled the outcome is written back to the
//! run and its job.

use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::{DeliveryRow, RunRow};

use crate::job::Status;

/// The answer that suppresses delivery.
pub const SILENT: &str = "[SILENT]";
/// Send attempts of one row: the first and up to three retries.
pub const MAX_ATTEMPTS: u32 = 4;
/// The first retry delay; each later one doubles.
pub const FIRST_BACKOFF_MS: i64 = 30_000;

/// Whether `output` asks for silence.
#[must_use]
pub fn is_silent(output: &str) -> bool {
    output.trim() == SILENT
}

/// The delivered text: `⏰ <name>`, the content and the pause footer when
/// `wrap`, else the content alone.
#[must_use]
pub fn frame(job_id: &str, job_name: &str, content: &str, wrap: bool) -> String {
    if !wrap {
        return content.to_owned();
    }
    format!(
        "⏰ {job_name}\n\n{}\n\njob {job_id} · prometheus cron pause {job_id}",
        content.trim_end()
    )
}

/// A failed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    /// Worth retrying (adapter down, rate limited).
    pub transient: bool,
    /// What went wrong.
    pub message: String,
}

/// Where deliveries go (the daemon's messaging gateway).
pub trait Sender: Send + Sync {
    /// Sends `body` to `target` for the profile; `warning` marks a failure
    /// notice.
    fn send<'a>(
        &'a self,
        target: &'a str,
        body: &'a str,
        warning: bool,
    ) -> pmt_agent::turn::model::BoxFuture<'a, Result<(), SendError>>;
}

/// A queued row for `run` to `target`.
#[must_use]
pub fn row(run: &RunRow, target: &str, body: &str, warning: bool, now_ms: i64) -> DeliveryRow {
    DeliveryRow {
        id: format!("dlv_{}", ulid::Ulid::generate()),
        run_id: run.id.clone(),
        job_id: run.job_id.clone(),
        target: target.to_owned(),
        body: body.to_owned(),
        warning,
        state: "pending".to_owned(),
        attempts: 0,
        next_attempt_at: now_ms,
        error: None,
        created_at: now_ms,
        updated_at: now_ms,
    }
}

/// Sends every row due at `now_ms`; returns how many were taken.
///
/// # Errors
///
/// Store errors.
pub async fn pump(store: &Store, sender: &dyn Sender, now_ms: i64) -> Result<usize, Error> {
    let mut taken = 0;
    while let Some(row) = store.cron_take_delivery(now_ms)? {
        taken += 1;
        let outcome = sender.send(&row.target, &row.body, row.warning).await;
        settle(store, &row, outcome, now_ms)?;
    }
    Ok(taken)
}

fn settle(
    store: &Store,
    row: &DeliveryRow,
    outcome: Result<(), SendError>,
    now_ms: i64,
) -> Result<(), Error> {
    match outcome {
        Ok(()) => store.cron_settle_delivery(&row.id, "sent", None, now_ms)?,
        Err(err) if err.transient && row.attempts < MAX_ATTEMPTS => {
            let backoff = FIRST_BACKOFF_MS << (row.attempts.saturating_sub(1)).min(16);
            tracing::info!(target = %row.target, error = %err.message, "cron delivery retried");
            return store.cron_settle_delivery(
                &row.id,
                "pending",
                Some(&err.message),
                now_ms + backoff,
            );
        }
        Err(err) => {
            tracing::warn!(target = %row.target, error = %err.message, "cron delivery failed");
            store.cron_settle_delivery(&row.id, "failed", Some(&err.message), now_ms)?;
        }
    }
    write_back(store, &row.run_id)
}

/// Settles rows left `sending` as `unknown` (never resent) and writes the
/// outcome back to their runs.
///
/// # Errors
///
/// Store errors.
pub fn recover(store: &Store) -> Result<(), Error> {
    for row in store.cron_orphaned_sends()? {
        write_back(store, &row.run_id)?;
    }
    Ok(())
}

/// Once every row of `run_id` is settled: `delivery_failed` (with the
/// first failure, or `unknown outcome`) when one failed or is unknown,
/// else `ok`. A run already in another terminal status keeps it.
///
/// # Errors
///
/// Store errors.
pub fn write_back(store: &Store, run_id: &str) -> Result<(), Error> {
    let rows = store.cron_deliveries_of(run_id)?;
    if rows
        .iter()
        .any(|r| matches!(r.state.as_str(), "pending" | "sending"))
    {
        return Ok(());
    }
    let failed = rows
        .iter()
        .find(|r| matches!(r.state.as_str(), "failed" | "unknown"));
    let (status, reason, error) = match failed {
        Some(r) if r.state == "unknown" => (
            Status::DeliveryFailed,
            Some("unknown outcome".to_owned()),
            Some("unknown outcome".to_owned()),
        ),
        Some(r) => (Status::DeliveryFailed, None, r.error.clone()),
        None => (Status::Ok, None, None),
    };
    let delivered: Vec<&str> = rows
        .iter()
        .filter(|r| r.state == "sent")
        .map(|r| r.target.as_str())
        .collect();
    let delivered = serde_json::to_string(&delivered).unwrap_or_default();
    store.cron_update_run(
        run_id,
        |run| {
            if run.status == Status::DeliveryQueued.as_str() || run.status == "running" {
                status.as_str().clone_into(&mut run.status);
                run.reason.clone_from(&reason);
                run.error.clone_from(&error);
            }
            run.delivered = Some(delivered.clone());
            Ok(())
        },
        |run, job| {
            // A later run's outcome is not overwritten by an older one's.
            if job
                .last_run_at
                .is_none_or(|at| at <= run.started_at.unwrap_or(at))
                && job.last_status.as_deref() == Some(Status::DeliveryQueued.as_str())
            {
                job.last_status = Some(run.status.clone());
                job.last_error.clone_from(&run.error);
            }
            Ok(())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{frame, is_silent};

    #[test]
    fn framing_and_silence() {
        assert_eq!(
            frame("a1b2c3d4e5f6", "nightly", "all green\n", true),
            "⏰ nightly\n\nall green\n\njob a1b2c3d4e5f6 · prometheus cron pause a1b2c3d4e5f6"
        );
        assert_eq!(frame("x", "n", "raw", false), "raw");
        assert!(is_silent("  [SILENT]\n"));
        assert!(!is_silent("[SILENT] but more"));
    }
}
