//! What follows a failed run (spec: scheduled-jobs, Failure streak and
//! review nudge; Automatic re-runs when the model was unreachable; Hold
//! through a closed provider usage window; Failure notice routing;
//! Agent-declared run failure).

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::incident::{MAX_ERROR, truncate};
use crate::job::{Job, Status};
use crate::schedule::Schedule;

/// The first line that declares a run failed.
pub const MARKER: &str = "[CRON_FAILURE]";

/// Delays of the automatic re-runs after an unreachable model.
pub const RETRY_LADDER_SECS: [u64; 3] = [300, 900, 1800];

/// The error an agent declared with [`MARKER`] alone on its first line.
#[must_use]
pub fn declared(response: &str) -> Option<String> {
    let (first, rest) = response.split_once('\n').unwrap_or((response, ""));
    (first.trim_end_matches('\r') == MARKER).then(|| truncate(rest.trim(), MAX_ERROR))
}

/// Whether `status` counts toward the failure streak.
#[must_use]
pub fn counts(status: Status) -> bool {
    matches!(
        status,
        Status::Failed | Status::TimedOut | Status::BlockedConfig
    )
}

/// The streak after a run ending in `status` (deliveries leave it as is).
#[must_use]
pub fn streak(previous: u32, status: Status) -> u32 {
    match status {
        s if counts(s) => previous.saturating_add(1),
        Status::Ok | Status::Silent => 0,
        _ => previous,
    }
}

/// The failure notice of `job`, in the profile's language: the error and,
/// for a recurring job at or past `threshold` consecutive failures, the
/// review nudge.
#[must_use]
pub fn notice(
    text: &dyn pmt_core::text::Catalog,
    job: &Job,
    status: Status,
    error: &str,
    streak: u32,
    threshold: u32,
) -> String {
    let key = if status == Status::TimedOut {
        "gateway.cron_interrupted"
    } else {
        "gateway.cron_failed"
    };
    let id = job.id();
    let mut body = text.text(
        key,
        &[("name", &job.row.name), ("id", &id), ("error", &error)],
    );
    if threshold > 0 && streak >= threshold && job.spec.schedule.recurring() {
        let nudge = text.text("gateway.cron_streak", &[("streak", &streak), ("id", &id)]);
        let _ = write!(body, "\n\n{nudge}");
    }
    body
}

/// When the next automatic re-run of a recurring job fires after an
/// unreachable model at `now`, given the re-runs done so far (`step`) and
/// its next natural occurrence; `None` once the ladder is exhausted or the
/// re-run would not come before the natural occurrence.
#[must_use]
pub fn next_retry(step: u32, now: Timestamp, next_natural: Option<i64>) -> Option<i64> {
    let delay = RETRY_LADDER_SECS.get(usize::try_from(step).ok()?)?;
    let at = now.as_millisecond() + i64::try_from(delay * 1000).ok()?;
    next_natural.is_none_or(|n| at < n).then_some(at)
}

/// How a job is held through a usage window closed until `until`: a
/// one-time re-run at `until` (`retry`), or the first natural occurrence
/// after it (`next`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// The re-run at the reopening instant.
    pub retry: Option<i64>,
    /// The job's next natural occurrence.
    pub next: Option<i64>,
}

/// Plans the hold of `job` until `until` (ms), from its next natural
/// occurrence `next` and its zone: a cron-kind job whose window reopens
/// at least half a period before its next natural occurrence re-runs once
/// at the reopening; otherwise the missed occurrences coalesce into the
/// first one after the hold.
#[must_use]
pub fn hold(job: &Job, until: i64, next: Option<i64>, zone: &TimeZone) -> Hold {
    let schedule = &job.spec.schedule;
    let at = crate::schedule::at_ms(until);
    if let (Schedule::Cron { .. }, Some(next_ms)) = (schedule, next) {
        let period = schedule.period_secs(at, zone).unwrap_or(0);
        let half = i64::try_from(period.saturating_mul(500)).unwrap_or(i64::MAX);
        if until.saturating_add(half) <= next_ms {
            return Hold {
                retry: Some(until),
                next,
            };
        }
    }
    let after = match next {
        Some(n) if n < until => schedule
            .next_after(
                crate::schedule::at_ms(until - 1),
                crate::schedule::at_ms(n),
                zone,
            )
            .ok()
            .flatten()
            .map(Timestamp::as_millisecond),
        other => other,
    };
    Hold {
        retry: None,
        next: after,
    }
}

#[cfg(test)]
mod tests {
    use super::{declared, next_retry, streak};
    use crate::job::Status;

    /// Scenario: Agent reports a failed subtask (the marker's parsing).
    #[test]
    fn the_marker_counts_only_alone_on_the_first_line() {
        assert_eq!(
            declared("[CRON_FAILURE]\nexport subagent exited: disk full").as_deref(),
            Some("export subagent exited: disk full")
        );
        assert_eq!(declared("all good [CRON_FAILURE]"), None);
        assert_eq!(declared("[CRON_FAILURE] x\nrest"), None);
        assert_eq!(declared("[CRON_FAILURE]").as_deref(), Some(""));
    }

    #[test]
    fn streaks_and_ladder() {
        assert_eq!(streak(2, Status::Failed), 3);
        assert_eq!(streak(2, Status::DeliveryFailed), 2);
        assert_eq!(streak(2, Status::Silent), 0);
        let now = jiff::Timestamp::from_millisecond(0).unwrap();
        assert_eq!(next_retry(0, now, None), Some(300_000));
        assert_eq!(next_retry(2, now, Some(10_000_000)), Some(1_800_000));
        assert_eq!(next_retry(3, now, None), None);
        assert_eq!(next_retry(0, now, Some(200_000)), None);
    }
}
