//! The next instant of a rule: its periods (years, months, weeks, days or
//! hours counted from the start in steps of `INTERVAL`) are expanded one
//! at a time into their wall-clock candidates, filtered by the `BY*`
//! parts, cut by `BYSETPOS`, and resolved in the zone like a cron
//! expression's times (a gap fires at the transition, a fold once).

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};

use super::{Freq, Rule};
use crate::schedule::next::resolve;

/// Periods looked at before a rule counts as never firing again.
const PERIODS: i64 = 10_000;

/// The first instant of `rule` started at `start` strictly after `base`;
/// `None` when no instant follows (none matches, or `COUNT` or `UNTIL`
/// ended the rule).
pub(crate) fn next_after(
    rule: &Rule,
    start: Timestamp,
    base: Timestamp,
    zone: &TimeZone,
) -> Result<Option<Timestamp>, String> {
    let first = start.to_zoned(zone.clone()).datetime();
    let from = base.max(start).to_zoned(zone.clone()).datetime();
    let skip = periods_between(rule.freq, first, from, rule.week_start)? / i64::from(rule.interval);
    // `COUNT` counts the instances after the start, so a counted rule is
    // walked from its first period.
    let begin = if rule.count.is_some() { 0 } else { skip };
    let mut walk = Walk {
        rule,
        start,
        base,
        zone,
        seen: 0,
    };
    for k in begin..skip + PERIODS {
        let Some(candidates) = period(rule, first, k * i64::from(rule.interval)) else {
            break;
        };
        match walk.scan(candidates)? {
            Step::Found(at) => return Ok(Some(at)),
            Step::Ended => return Ok(None),
            Step::More => {}
        }
    }
    Ok(None)
}

/// What one period's candidates gave.
enum Step {
    Found(Timestamp),
    Ended,
    More,
}

/// The instances walked so far towards the first one after `base`.
struct Walk<'a> {
    rule: &'a Rule,
    start: Timestamp,
    base: Timestamp,
    zone: &'a TimeZone,
    /// Instances after the start seen so far (what `COUNT` bounds).
    seen: u32,
}

impl Walk<'_> {
    fn scan(&mut self, candidates: Vec<DateTime>) -> Result<Step, String> {
        for dt in candidates {
            let at = resolve(self.zone, dt)?;
            // The start is the job's creation, never one of its runs.
            if at <= self.start {
                continue;
            }
            if ended(self.rule, self.seen, dt, at) {
                return Ok(Step::Ended);
            }
            self.seen += 1;
            if at > self.base {
                return Ok(Step::Found(at));
            }
        }
        Ok(Step::More)
    }
}

/// Whether the instance at `dt` (`at`), after `seen` earlier ones, is
/// past the rule's `COUNT` or `UNTIL`.
fn ended(rule: &Rule, seen: u32, dt: DateTime, at: Timestamp) -> bool {
    rule.count.is_some_and(|count| seen >= count)
        || rule.until.is_some_and(|until| until.passed(dt, at))
}

/// Whole periods of `freq` from `first`'s period to `at`'s.
fn periods_between(
    freq: Freq,
    first: DateTime,
    at: DateTime,
    week_start: i8,
) -> Result<i64, String> {
    let error = |e: jiff::Error| e.to_string();
    let days = |a: Date, b: Date| a.until(b).map(|s| i64::from(s.get_days())).map_err(error);
    let months =
        |a: Date, b: Date| i64::from(b.year() - a.year()) * 12 + i64::from(b.month() - a.month());
    Ok(match freq {
        Freq::Yearly => i64::from(at.year() - first.year()),
        Freq::Monthly => months(first.date(), at.date()),
        Freq::Weekly => {
            days(
                week_of(first.date(), week_start),
                week_of(at.date(), week_start),
            )? / 7
        }
        Freq::Daily => days(first.date(), at.date())?,
        Freq::Hourly => days(first.date(), at.date())? * 24 + i64::from(at.hour() - first.hour()),
    }
    .max(0))
}

/// The first day of the week holding `date`.
fn week_of(date: Date, week_start: i8) -> Date {
    let back = (date.weekday().to_monday_zero_offset() - week_start).rem_euclid(7);
    date.checked_sub(i64::from(back).days()).unwrap_or(date)
}

fn weekday(date: Date) -> i8 {
    date.weekday().to_monday_zero_offset()
}

/// The candidates of the period `offset` periods after the first one, in
/// order; `None` past the calendar's end.
fn period(rule: &Rule, first: DateTime, offset: i64) -> Option<Vec<DateTime>> {
    let (dates, hours) = match rule.freq {
        Freq::Yearly => {
            let year = first
                .date()
                .first_of_year()
                .checked_add(offset.years())
                .ok()?;
            (year_days(rule, year, first.date()), times(rule, first))
        }
        Freq::Monthly => {
            let month = first
                .date()
                .first_of_month()
                .checked_add(offset.months())
                .ok()?;
            (month_days(rule, month, first.day()), times(rule, first))
        }
        Freq::Weekly => {
            let week = week_of(first.date(), rule.week_start)
                .checked_add((offset * 7).days())
                .ok()?;
            (week_days(rule, week, first.date()), times(rule, first))
        }
        Freq::Daily => {
            let day = first.date().checked_add(offset.days()).ok()?;
            (single_day(rule, day), times(rule, first))
        }
        Freq::Hourly => {
            let hour = first
                .with()
                .minute(0)
                .second(0)
                .subsec_nanosecond(0)
                .build()
                .ok()?
                .checked_add(offset.hours())
                .ok()?;
            let wanted = rule.by_hour.is_empty() || rule.by_hour.contains(&hour.hour());
            let minutes = minutes(rule, first);
            let hours = if wanted {
                minutes.iter().map(|m| (hour.hour(), *m)).collect()
            } else {
                Vec::new()
            };
            (single_day(rule, hour.date()), hours)
        }
    };
    let mut candidates: Vec<DateTime> = dates
        .iter()
        .flat_map(|date| hours.iter().map(|(h, m)| date.at(*h, *m, 0, 0)))
        .collect();
    candidates.sort();
    Some(set_positions(rule, candidates))
}

fn minutes(rule: &Rule, first: DateTime) -> Vec<i8> {
    if rule.by_minute.is_empty() {
        vec![first.minute()]
    } else {
        rule.by_minute.iter().copied().collect()
    }
}

/// The times of day of a day-or-longer period.
fn times(rule: &Rule, first: DateTime) -> Vec<(i8, i8)> {
    let hours: Vec<i8> = if rule.by_hour.is_empty() {
        vec![first.hour()]
    } else {
        rule.by_hour.iter().copied().collect()
    };
    let minutes = minutes(rule, first);
    hours
        .iter()
        .flat_map(|h| minutes.iter().map(move |m| (*h, *m)))
        .collect()
}

/// `BYSETPOS`: the nth candidates of the period (negative: from the end).
fn set_positions(rule: &Rule, candidates: Vec<DateTime>) -> Vec<DateTime> {
    if rule.by_set_pos.is_empty() {
        return candidates;
    }
    let len = i64::try_from(candidates.len()).unwrap_or(i64::MAX);
    let mut picked: Vec<DateTime> = rule
        .by_set_pos
        .iter()
        .filter_map(|pos| {
            let pos = i64::from(*pos);
            let index = if pos > 0 { pos - 1 } else { len + pos };
            usize::try_from(index)
                .ok()
                .and_then(|i| candidates.get(i))
                .copied()
        })
        .collect();
    picked.sort();
    picked.dedup();
    picked
}

/// The days of month `month` (its first day) the rule names; `day` when
/// it names none.
fn month_days(rule: &Rule, month: Date, day: i8) -> Vec<Date> {
    if !rule.by_month.is_empty() && !rule.by_month.contains(&month.month()) {
        return Vec::new();
    }
    let last = month.days_in_month();
    let all = (1..=last).filter_map(|d| month.with().day(d).build().ok());
    let by_month_day = |d: &Date| {
        rule.by_month_day.iter().any(|n| {
            let n = if *n > 0 { *n } else { last + 1 + *n };
            n == d.day()
        })
    };
    let days: Vec<Date> = match (rule.by_month_day.is_empty(), rule.by_day.is_empty()) {
        (true, true) => all.filter(|d| d.day() == day).collect(),
        (false, true) => all.filter(by_month_day).collect(),
        (true, false) => all.filter(|d| by_day_in(rule, month, *d)).collect(),
        (false, false) => all
            .filter(|d| by_month_day(d) && by_day_in(rule, month, *d))
            .collect(),
    };
    days
}

/// Whether `date` is a `BYDAY` day of its month (ordinals count within
/// the month).
fn by_day_in(rule: &Rule, month: Date, date: Date) -> bool {
    let last = month.days_in_month();
    rule.by_day.iter().any(|by| {
        if weekday(date) != by.day {
            return false;
        }
        match by.nth {
            None => true,
            Some(n) if n > 0 => (date.day() - 1) / 7 + 1 == n,
            Some(n) => (last - date.day()) / 7 + 1 == -n,
        }
    })
}

/// The days of the year starting `year` the rule names.
fn year_days(rule: &Rule, year: Date, first: Date) -> Vec<Date> {
    let names_days = !rule.by_month_day.is_empty() || !rule.by_day.is_empty();
    let months: Vec<i8> = match (rule.by_month.is_empty(), names_days) {
        (false, _) => rule.by_month.iter().copied().collect(),
        (true, true) => (1..=12).collect(),
        (true, false) => vec![first.month()],
    };
    months
        .into_iter()
        .filter_map(|m| year.with().month(m).build().ok())
        .flat_map(|month| month_days(rule, month, first.day()))
        .collect()
}

/// The days of the week starting `week` the rule names; `first`'s
/// weekday when it names none.
fn week_days(rule: &Rule, week: Date, first: Date) -> Vec<Date> {
    (0..7)
        .filter_map(|d| week.checked_add(i64::from(d).days()).ok())
        .filter(|d| rule.by_month.is_empty() || rule.by_month.contains(&d.month()))
        .filter(|d| {
            if rule.by_day.is_empty() {
                weekday(*d) == weekday(first)
            } else {
                rule.by_day.iter().any(|by| by.day == weekday(*d))
            }
        })
        .collect()
}

/// `day` when the rule's month, month-day and weekday filters keep it.
fn single_day(rule: &Rule, day: Date) -> Vec<Date> {
    let last = day.days_in_month();
    let keep = (rule.by_month.is_empty() || rule.by_month.contains(&day.month()))
        && (rule.by_month_day.is_empty()
            || rule.by_month_day.iter().any(|n| {
                let n = if *n > 0 { *n } else { last + 1 + *n };
                n == day.day()
            }))
        && (rule.by_day.is_empty() || rule.by_day.iter().any(|by| by.day == weekday(day)));
    if keep { vec![day] } else { Vec::new() }
}
