//! Next-run computation (spec: scheduled-jobs, Next-run computation):
//! intervals count from the previous scheduled instant, cron expressions
//! run on the zone's wall clock. A wall-clock time skipped by a
//! spring-forward gap fires at the first valid instant after it (the
//! transition); a time repeated by a fall-back fold fires once, at its
//! first occurrence.

use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{Timestamp, ToSpan as _};

use super::CronExpr;

/// Days searched for a matching date (covers 29 February).
const SEARCH_DAYS: i32 = 366 * 8;

/// `anchor + k * period` for the smallest `k >= 1` past `base`.
pub(super) fn interval_after(every_secs: u64, base: Timestamp, anchor: Timestamp) -> Timestamp {
    let period = i64::try_from(every_secs).unwrap_or(i64::MAX).max(1);
    let elapsed = (base.as_second() - anchor.as_second()).max(0);
    let steps = elapsed / period + 1;
    anchor
        .checked_add(steps.saturating_mul(period).seconds())
        .unwrap_or(Timestamp::MAX)
}

/// The instant of civil `dt` in `zone`: a gap resolves to the transition,
/// a fold to its earlier instant.
pub(super) fn resolve(zone: &TimeZone, dt: DateTime) -> Result<Timestamp, String> {
    let error = |e: jiff::Error| e.to_string();
    match zone.to_ambiguous_timestamp(dt).offset() {
        AmbiguousOffset::Unambiguous { offset } => offset.to_timestamp(dt).map_err(error),
        AmbiguousOffset::Fold { before, .. } => before.to_timestamp(dt).map_err(error),
        AmbiguousOffset::Gap { before, after } => {
            // `dt` read with the later offset lies before the transition,
            // with the earlier offset after it: bisect to the second.
            let mut lo = after.to_timestamp(dt).map_err(error)?.as_second();
            let mut hi = before.to_timestamp(dt).map_err(error)?.as_second();
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                let at = Timestamp::from_second(mid).map_err(error)?;
                if zone.to_offset(at) == after {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            Timestamp::from_second(hi).map_err(error)
        }
    }
}

/// The first instant of `expr` strictly after `base` in `zone`; `None`
/// when no date within the search window matches.
pub(super) fn cron_after(
    expr: &CronExpr,
    base: Timestamp,
    zone: &TimeZone,
) -> Result<Option<Timestamp>, String> {
    let start = base.to_zoned(zone.clone()).date();
    let times = expr.times();
    for offset in 0..SEARCH_DAYS {
        let Ok(date) = start.checked_add(offset.days()) else {
            break;
        };
        if !expr.matches_date(date) {
            continue;
        }
        for &(hour, minute) in &times {
            let dt = date.at(hour, minute, 0, 0);
            let at = resolve(zone, dt)?;
            if at > base {
                return Ok(Some(at));
            }
        }
    }
    Ok(None)
}
