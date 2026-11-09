//! The schedule grammar (spec: scheduled-jobs, Unambiguous schedule
//! grammar): every input parses to exactly one of `interval`
//! (`every <duration>`), `cron` (five fields or an exact day/time phrase)
//! or `once` (RFC 3339, `at <time>`, `tomorrow at <time>`, `in
//! <duration>`). A bare duration is refused instead of guessed, and
//! nothing shorter than a minute is accepted.

mod cronexpr;
mod echo;
mod next;
mod phrase;

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

pub use cronexpr::CronExpr;
pub use echo::{Echo, Instant};

use crate::duration;

/// The shortest interval or delay.
pub const MIN_SECS: u64 = 60;

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `every_secs`, anchored on the previous scheduled instant.
    Interval {
        /// The period.
        every_secs: u64,
    },
    /// A five-field cron expression evaluated on the zone's wall clock.
    Cron {
        /// The canonical expression.
        expr: String,
    },
    /// One instant.
    Once {
        /// Unix milliseconds.
        at_ms: i64,
    },
}

/// What the accepted forms are, for the `invalid schedule` message.
const FORMS: &str = "expected 'every <duration>' (every 30m), a cron expression \
    (0 9 * * 1-5) or day/time phrase (weekdays at 9:30), an RFC 3339 time, 'at <time>', \
    'tomorrow at <time>' or 'in <duration>' (in 30m); durations are a number and one of \
    s, m, h, d, w";

fn invalid(input: &str) -> Error {
    Error::new(
        &codes::CRON_INVALID_SCHEDULE,
        format!("invalid schedule '{input}': {FORMS}"),
    )
}

fn refuse(message: impl Into<String>) -> Error {
    Error::new(&codes::CRON_INVALID_SCHEDULE, message)
}

fn ms(ts: Timestamp) -> i64 {
    ts.as_millisecond()
}

/// A timestamp from Unix milliseconds.
#[must_use]
pub fn at_ms(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

impl Schedule {
    /// Parses `input` at `now` in `zone` (the zone resolves wall-clock
    /// forms such as `tomorrow at 9am`).
    ///
    /// # Errors
    ///
    /// `cron.invalid_schedule`: ambiguous, too short, in the past or not
    /// in the grammar.
    pub fn parse(input: &str, now: Timestamp, zone: &TimeZone) -> Result<Self, Error> {
        let text = input.split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = text.to_ascii_lowercase();
        if lower.is_empty() {
            return Err(invalid(input));
        }
        if duration::parse(&lower).is_some() {
            return Err(refuse(format!(
                "ambiguous schedule '{text}': use 'every {lower}' for a recurring interval or \
                 'in {lower}' for a one-shot"
            )));
        }
        if let Some(rest) = lower.strip_prefix("every ")
            && let Some(secs) = duration::parse(rest)
        {
            if secs < MIN_SECS {
                return Err(refuse("interval must be at least 60s"));
            }
            return Ok(Self::Interval { every_secs: secs });
        }
        if let Some(rest) = lower.strip_prefix("in ") {
            let secs = duration::parse(rest).ok_or_else(|| invalid(input))?;
            if secs < MIN_SECS {
                return Err(refuse("delay must be at least 60s"));
            }
            let at = now
                .checked_add(i64::try_from(secs).unwrap_or(i64::MAX).seconds())
                .map_err(|_| invalid(input))?;
            return Ok(Self::Once { at_ms: ms(at) });
        }
        if let Some(schedule) = Self::cron(&lower)? {
            return Ok(schedule);
        }
        let at = once_instant(&text, &lower, now, zone).ok_or_else(|| invalid(input))?;
        if at <= now {
            return Err(refuse(format!("schedule '{text}' is in the past ({at})")));
        }
        Ok(Self::Once { at_ms: ms(at) })
    }

    /// A cron expression or day/time phrase; `Ok(None)` when `lower` is
    /// neither.
    fn cron(lower: &str) -> Result<Option<Self>, Error> {
        let expr = match phrase::cron_of(lower) {
            Some(expr) => expr,
            None if lower.split_whitespace().count() == 5 => lower.to_owned(),
            None => return Ok(None),
        };
        let parsed = CronExpr::parse(&expr)
            .map_err(|why| refuse(format!("invalid cron expression '{expr}': {why}")))?;
        let schedule = Self::Cron {
            expr: parsed.text().to_owned(),
        };
        // An expression no date matches (`0 0 30 2 *`) would never fire.
        if next::cron_after(&parsed, Timestamp::UNIX_EPOCH, &TimeZone::UTC)
            .ok()
            .flatten()
            .is_none()
        {
            return Err(refuse(format!("cron expression '{expr}' never fires")));
        }
        Ok(Some(schedule))
    }

    /// `interval`, `cron` or `once`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Interval { .. } => "interval",
            Self::Cron { .. } => "cron",
            Self::Once { .. } => "once",
        }
    }

    /// The canonical text (`every 30m`, `0 9 * * 1-5`, an RFC 3339 UTC
    /// time), which parses back to the same schedule.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Interval { every_secs } => format!("every {}", duration::format(*every_secs)),
            Self::Cron { expr } => expr.clone(),
            Self::Once { at_ms: at } => at_ms(*at).to_string(),
        }
    }

    /// Whether it fires more than once.
    #[must_use]
    pub const fn recurring(&self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    /// The first instant strictly after `base`; `anchor` is the previous
    /// scheduled instant an interval counts from (so runs do not drift).
    /// `None` when a one-shot instant has passed.
    ///
    /// # Errors
    ///
    /// The reason the instant cannot be computed (an unparseable stored
    /// expression).
    pub fn next_after(
        &self,
        base: Timestamp,
        anchor: Timestamp,
        zone: &TimeZone,
    ) -> Result<Option<Timestamp>, String> {
        match self {
            Self::Interval { every_secs } => {
                Ok(Some(next::interval_after(*every_secs, base, anchor)))
            }
            Self::Cron { expr } => next::cron_after(&CronExpr::parse(expr)?, base, zone),
            Self::Once { at_ms: at } => Ok(Some(at_ms(*at)).filter(|t| *t > base)),
        }
    }

    /// Seconds between two consecutive occurrences around `base` (the
    /// grace default is half of it); `None` for a one-shot.
    #[must_use]
    pub fn period_secs(&self, base: Timestamp, zone: &TimeZone) -> Option<u64> {
        match self {
            Self::Interval { every_secs } => Some(*every_secs),
            Self::Cron { .. } => {
                let first = self.next_after(base, base, zone).ok()??;
                let second = self.next_after(first, first, zone).ok()??;
                u64::try_from((second - first).get_seconds()).ok()
            }
            Self::Once { .. } => None,
        }
    }
}

/// The instant of a one-shot form other than `in <duration>`.
fn once_instant(text: &str, lower: &str, now: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
    if let Ok(at) = text.parse::<Timestamp>() {
        return Some(at);
    }
    let today = now.to_zoned(zone.clone()).date();
    if let Some(rest) = lower.strip_prefix("tomorrow at ") {
        let (hour, minute) = phrase::time_of_day(rest)?;
        return local(zone, today.tomorrow().ok()?, hour, minute);
    }
    let rest = lower.strip_prefix("at ")?;
    if let Some((day, time)) = rest.split_once(' ')
        && let Ok(date) = day.parse::<Date>()
    {
        let (hour, minute) = phrase::time_of_day(time)?;
        return local(zone, date, hour, minute);
    }
    let (hour, minute) = phrase::time_of_day(rest)?;
    let at = local(zone, today, hour, minute)?;
    // `at 9am` after 9 am means the next 9 am.
    if at > now {
        Some(at)
    } else {
        local(zone, today.tomorrow().ok()?, hour, minute)
    }
}

fn local(zone: &TimeZone, date: Date, hour: i8, minute: i8) -> Option<Timestamp> {
    let dt = DateTime::from_parts(date, jiff::civil::time(hour, minute, 0, 0));
    next::resolve(zone, dt).ok()
}

#[cfg(test)]
mod tests;
