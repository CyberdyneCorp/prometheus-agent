//! Whether an automatic compaction may run now (spec: context-compression,
//! Trigger decision and blocking reasons; Provider usage beats rough
//! estimates; Summary failure handling).
//!
//! The guards live in `state.db` ([`CompressionState`]) so a restart
//! honours them: a failed summary arms a cooldown escalating 60 s, 300 s,
//! 900 s; an attempt that commits nothing for another reason (nothing to
//! summarize, a concurrent rewrite) arms a 60 s backoff; and two
//! consecutive compactions that the provider's next reported usage shows
//! reclaimed less than 10% trip the `ineffective` breaker, which only a
//! manual `/compress` resets.

use std::fmt;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_store::CompressionState;

/// Cooldowns after the 1st, 2nd and 3rd+ consecutive failed summary.
pub const COOLDOWNS: [Duration; 3] = [
    Duration::from_mins(1),
    Duration::from_mins(5),
    Duration::from_mins(15),
];
/// The wait after an attempt that committed nothing for another reason.
pub const BACKOFF: Duration = Duration::from_mins(1);
/// Consecutive ineffective compactions that trip the breaker.
pub const BREAKER_TRIPS: u32 = 2;
/// A compaction reclaiming less than this share is ineffective.
pub const MIN_RECLAIM: f64 = 0.10;

/// Why an automatic compaction may not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// A failed summary's cooldown, seconds remaining.
    Cooldown(u64),
    /// A backoff, seconds remaining.
    Backoff(u64),
    /// The breaker tripped.
    Ineffective,
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cooldown(secs) => write!(f, "cooldown:{secs}"),
            Self::Backoff(secs) => write!(f, "backoff:{secs}"),
            Self::Ineffective => f.write_str("ineffective"),
        }
    }
}

/// The outcome of [`should_compress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Compact now.
    Run,
    /// The context is below the trigger.
    BelowThreshold,
    /// Over the trigger, but a guard holds.
    Blocked(Blocked),
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run => f.write_str("run"),
            Self::BelowThreshold => f.write_str("below_threshold"),
            Self::Blocked(why) => write!(f, "blocked({why})"),
        }
    }
}

/// Whole seconds from `now` until `until` (rounded up); `None` once passed.
fn remaining(until: Option<Timestamp>, now: Timestamp) -> Option<u64> {
    let left = until?.duration_since(now);
    if left <= SignedDuration::ZERO {
        return None;
    }
    let millis = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    Some(millis.div_ceil(1000))
}

/// Whether `tokens` of context should be compacted now under `state`.
#[must_use]
pub fn should_compress(
    tokens: u64,
    trigger: u64,
    state: &CompressionState,
    now: Timestamp,
) -> Decision {
    if tokens < trigger {
        return Decision::BelowThreshold;
    }
    if let Some(secs) = remaining(state.cooldown_until, now) {
        return Decision::Blocked(Blocked::Cooldown(secs));
    }
    if let Some(secs) = remaining(state.backoff_until, now) {
        return Decision::Blocked(Blocked::Backoff(secs));
    }
    if state.ineffective >= BREAKER_TRIPS {
        return Decision::Blocked(Blocked::Ineffective);
    }
    Decision::Run
}

fn after(now: Timestamp, wait: Duration) -> Timestamp {
    now.checked_add(wait).unwrap_or(now)
}

/// A failed, timed-out or truncated summary: the next cooldown.
pub fn summary_failed(state: &mut CompressionState, now: Timestamp) -> Duration {
    state.summary_failures = state.summary_failures.saturating_add(1);
    let step = usize::try_from(state.summary_failures - 1).unwrap_or(usize::MAX);
    let wait = COOLDOWNS[step.min(COOLDOWNS.len() - 1)];
    state.cooldown_until = Some(after(now, wait));
    wait
}

/// An attempt that committed nothing for a reason other than the summary.
pub fn back_off(state: &mut CompressionState, now: Timestamp) {
    state.backoff_until = Some(after(now, BACKOFF));
}

/// A committed compaction of `tokens_before` tokens; `summarized` when the
/// model wrote the summary (a fallback keeps the failure count).
pub fn committed(state: &mut CompressionState, tokens_before: u64, summarized: bool) {
    if summarized {
        state.summary_failures = 0;
        state.cooldown_until = None;
    }
    state.backoff_until = None;
    state.pending_before = Some(tokens_before);
}

/// The provider's first reported input after a commit judges it; returns
/// whether `state` changed.
pub fn observe_usage(state: &mut CompressionState, reported: u64) -> bool {
    let Some(before) = state.pending_before.take() else {
        return false;
    };
    let reclaimed = before.saturating_sub(reported);
    #[allow(clippy::cast_precision_loss)] // token counts stay far below 2^52
    let effective = before > 0 && reclaimed as f64 / before as f64 >= MIN_RECLAIM;
    state.ineffective = if effective {
        0
    } else {
        state.ineffective.saturating_add(1)
    };
    true
}

/// `/compress`: clears the cooldown, the backoff and the breaker.
pub fn manual(state: &mut CompressionState) {
    state.cooldown_until = None;
    state.backoff_until = None;
    state.ineffective = 0;
}

/// How the context size is known before a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// The provider's reported input of the latest request plus estimates
    /// of what was added since.
    Reported(u64),
    /// Only a local estimate.
    Estimated(u64),
}

/// What the pre-request gate does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Below the trigger: send.
    Send,
    /// Over the trigger: decide with [`should_compress`] on these tokens.
    Evaluate(u64),
    /// Only a rough estimate is over: send uncompressed and decide on the
    /// provider's reported usage.
    MeasureFirst,
}

/// Provider usage beats estimates: an estimate alone triggers only for a
/// provider known to omit usage.
#[must_use]
pub fn gate(measure: Measure, omits_usage: bool, trigger: u64) -> Gate {
    match measure {
        Measure::Reported(tokens) | Measure::Estimated(tokens) if tokens < trigger => Gate::Send,
        Measure::Reported(tokens) => Gate::Evaluate(tokens),
        Measure::Estimated(tokens) if omits_usage => Gate::Evaluate(tokens),
        Measure::Estimated(_) => Gate::MeasureFirst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    fn secs(n: i64) -> Timestamp {
        now().checked_add(SignedDuration::from_secs(n)).unwrap()
    }

    #[test]
    fn decision_table() {
        let clear = CompressionState::default();
        let cooling = CompressionState {
            cooldown_until: Some(secs(120)),
            ..CompressionState::default()
        };
        let backing = CompressionState {
            backoff_until: Some(secs(30)),
            ..CompressionState::default()
        };
        let tripped = CompressionState {
            ineffective: 2,
            ..CompressionState::default()
        };
        let expired = CompressionState {
            cooldown_until: Some(secs(-1)),
            ..CompressionState::default()
        };
        let rows = [
            (99, &clear, "below_threshold"),
            (100, &clear, "run"),
            (150, &cooling, "blocked(cooldown:120)"),
            (10, &cooling, "below_threshold"),
            (150, &backing, "blocked(backoff:30)"),
            (150, &tripped, "blocked(ineffective)"),
            (150, &expired, "run"),
        ];
        for (tokens, state, expected) in rows {
            assert_eq!(
                should_compress(tokens, 100, state, now()).to_string(),
                expected
            );
        }
    }

    #[test]
    fn three_timeouts_escalate_the_cooldown() {
        let mut state = CompressionState::default();
        let waits: Vec<u64> = (0..4)
            .map(|_| summary_failed(&mut state, now()).as_secs())
            .collect();
        assert_eq!(waits, [60, 300, 900, 900]);
        assert_eq!(state.cooldown_until, Some(secs(900)));
        committed(&mut state, 1_000, true);
        assert_eq!(state.summary_failures, 0);
        assert_eq!(summary_failed(&mut state, now()).as_secs(), 60);
    }

    #[test]
    fn two_ineffective_compactions_trip_the_breaker_and_manual_resets_it() {
        let mut state = CompressionState::default();
        for _ in 0..2 {
            committed(&mut state, 100_000, true);
            assert!(observe_usage(&mut state, 95_000)); // 5% reclaimed
        }
        assert_eq!(
            should_compress(200_000, 100_000, &state, now()),
            Decision::Blocked(Blocked::Ineffective)
        );
        assert!(!observe_usage(&mut state, 1), "nothing pending");
        manual(&mut state);
        assert_eq!(
            should_compress(200_000, 100_000, &state, now()),
            Decision::Run
        );
        committed(&mut state, 100_000, true);
        observe_usage(&mut state, 95_000);
        committed(&mut state, 100_000, true);
        observe_usage(&mut state, 50_000); // effective: the streak restarts
        assert_eq!(state.ineffective, 0);
    }

    #[test]
    fn provider_usage_beats_estimates() {
        assert_eq!(
            gate(Measure::Estimated(150), false, 100),
            Gate::MeasureFirst
        );
        assert_eq!(
            gate(Measure::Estimated(150), true, 100),
            Gate::Evaluate(150)
        );
        assert_eq!(
            gate(Measure::Reported(150), false, 100),
            Gate::Evaluate(150)
        );
        assert_eq!(gate(Measure::Reported(50), false, 100), Gate::Send);
        assert_eq!(gate(Measure::Estimated(50), false, 100), Gate::Send);
    }
}
