//! Failure incidents (spec: scheduled-jobs, Failure incidents with
//! alert-once and acknowledge): each failed run is recorded under its job
//! and a normalized error signature, and only the first failure of a
//! signature (plus one reminder) is announced.
//!
//! States: `detected → alerted → resolved | closed`. A success resolves
//! the job's open incidents (a later repeat re-opens and alerts again);
//! an acknowledged (`closed`) signature stays silent for good.

use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::IncidentRow;

/// Stored error text at most.
pub const MAX_ERROR: usize = 2 * 1024;

/// Unit words a number is dropped with.
const UNITS: &[&str] = &[
    "ns", "us", "µs", "ms", "s", "sec", "secs", "second", "seconds", "m", "min", "mins", "minute",
    "minutes", "h", "hr", "hrs", "hour", "hours", "d", "day", "days",
];

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `30s`, `1.5ms`: a number glued to a unit.
fn is_duration(token: &str) -> bool {
    let split = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    let (number, unit) = token.split_at(split);
    is_number(number) && UNITS.contains(&unit)
}

/// The signature of `error`: lower-cased, whitespace collapsed, numbers
/// with duration units removed (so `timed out after 30s` and `timed out
/// after 45 s` are one incident).
#[must_use]
pub fn signature(error: &str) -> String {
    let lower = error.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut kept = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].trim_end_matches([',', '.', ';', ')']);
        if is_duration(token) {
            i += 1;
            continue;
        }
        let next = tokens
            .get(i + 1)
            .map(|t| t.trim_end_matches([',', '.', ';', ')']));
        if is_number(token) && next.is_some_and(|n| UNITS.contains(&n)) {
            i += 2;
            continue;
        }
        kept.push(tokens[i]);
        i += 1;
    }
    kept.join(" ")
}

/// `text` cut to at most `max` bytes on a character boundary.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// What one failure means for an incident.
fn decide(
    found: Option<IncidentRow>,
    fresh: IncidentRow,
    now_ms: i64,
    repeat_ms: i64,
) -> (IncidentRow, bool) {
    let Some(mut row) = found else {
        return (fresh, true);
    };
    row.occurrences = row.occurrences.saturating_add(1);
    row.last_seen_at = now_ms;
    row.updated_at = now_ms;
    row.error = fresh.error;
    row.job_name = fresh.job_name;
    let announce = match row.state.as_str() {
        "closed" => false,
        "alerted" if repeat_ms == 0 => true,
        "alerted" => {
            let due = row
                .alerted_at
                .is_none_or(|at| now_ms.saturating_sub(at) >= repeat_ms);
            let remind = due && !row.reminded;
            row.reminded |= remind;
            remind
        }
        // detected, or resolved and seen again: announce at once.
        _ => {
            row.reminded = false;
            true
        }
    };
    if announce {
        "alerted".clone_into(&mut row.state);
        row.alerted_at = Some(now_ms);
    }
    (row, announce)
}

/// Records a failed run of `job_id` with `error` (already redacted);
/// whether its notice is delivered. An unreadable incident table fails
/// open: the notice is delivered.
#[must_use]
pub fn on_failure(
    store: &Store,
    job_id: &str,
    job_name: &str,
    error: &str,
    now_ms: i64,
    repeat_secs: u64,
) -> bool {
    let error = truncate(error, MAX_ERROR);
    let signature = signature(&error);
    let repeat_ms = i64::try_from(repeat_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let fresh = IncidentRow {
        id: format!("inc_{}", ulid::Ulid::generate()),
        job_id: job_id.to_owned(),
        job_name: job_name.to_owned(),
        signature: signature.clone(),
        state: "alerted".to_owned(),
        error,
        occurrences: 1,
        reminded: false,
        first_seen_at: now_ms,
        last_seen_at: now_ms,
        alerted_at: Some(now_ms),
        updated_at: now_ms,
    };
    let recorded = store.cron_record_incident(job_id, &signature, |found| {
        Ok(decide(found, fresh.clone(), now_ms, repeat_ms))
    });
    recorded.unwrap_or_else(|err: Error| {
        tracing::warn!(error = %err, "cron incident table unreadable; delivering the notice");
        true
    })
}

/// Resolves the open incidents of `job_id` after a successful run.
pub fn on_success(store: &Store, job_id: &str, now_ms: i64) {
    if let Err(err) = store.cron_resolve_incidents(job_id, now_ms) {
        tracing::warn!(error = %err, "cron incidents not resolved");
    }
}

#[cfg(test)]
mod tests {
    use super::signature;

    #[test]
    fn signatures_ignore_durations_and_spacing() {
        assert_eq!(
            signature("Timed out after 30s"),
            signature("timed  out after 45 s")
        );
        assert_eq!(signature("Connection refused"), "connection refused");
        assert_ne!(signature("disk full"), signature("connection refused"));
        assert_eq!(signature("exit 3 (code)"), "exit 3 (code)");
    }
}
