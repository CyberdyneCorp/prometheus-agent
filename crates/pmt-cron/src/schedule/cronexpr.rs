//! Five-field cron expressions (`minute hour day-of-month month
//! day-of-week`): `*`, numbers, `a-b` ranges, `/step`, comma lists, month
//! names `jan`-`dec` and weekday names `sun`-`sat` (`7` is Sunday too).
//! As in Vixie cron, when both day fields are restricted a day matching
//! either one matches.

use jiff::civil::{Date, Weekday};

/// A parsed expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
    text: String,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
}

const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
    },
    Field {
        name: "day of month",
        min: 1,
        max: 31,
        names: &[],
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: &MONTHS,
    },
    Field {
        name: "day of week",
        min: 0,
        max: 7,
        names: &WEEKDAYS,
    },
];

impl Field {
    fn value(&self, text: &str) -> Result<u32, String> {
        if let Some(i) = self.names.iter().position(|n| *n == text) {
            // Month names start at 1, weekday names at 0 (Sunday).
            let base = u32::from(self.min == 1);
            return Ok(u32::try_from(i).unwrap_or(0) + base);
        }
        let value: u32 = text
            .parse()
            .map_err(|_| format!("{}: '{text}' is not a number", self.name))?;
        if value < self.min || value > self.max {
            return Err(format!(
                "{}: {value} is outside {}-{}",
                self.name, self.min, self.max
            ));
        }
        Ok(value)
    }

    /// The set of values of one comma-separated part.
    fn part(&self, part: &str) -> Result<u64, String> {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("{}: bad step '{step}'", self.name))?;
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (self.min, self.max)
        } else if let Some((a, b)) = range.split_once('-') {
            (self.value(a)?, self.value(b)?)
        } else {
            let start = self.value(range)?;
            // `5/15` runs from 5 to the end of the field.
            (start, if part.contains('/') { self.max } else { start })
        };
        if lo > hi {
            return Err(format!("{}: empty range '{range}'", self.name));
        }
        Ok((lo..=hi)
            .step_by(step as usize)
            .fold(0, |set, v| set | (1 << v)))
    }

    fn parse(&self, text: &str) -> Result<u64, String> {
        text.split(',')
            .try_fold(0, |set, part| Ok(set | self.part(part)?))
    }
}

impl CronExpr {
    /// Parses five whitespace-separated fields.
    ///
    /// # Errors
    ///
    /// Which field is wrong and why.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lower = text.to_ascii_lowercase();
        let fields: Vec<&str> = lower.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!("expected 5 fields, found {}", fields.len()));
        }
        let sets: Vec<u64> = FIELDS
            .iter()
            .zip(&fields)
            .map(|(field, text)| field.parse(text))
            .collect::<Result<_, _>>()?;
        // Day-of-week 7 is Sunday.
        let weekdays = (sets[4] | (sets[4] >> 7)) & 0x7f;
        Ok(Self {
            minutes: sets[0],
            hours: u32::try_from(sets[1]).unwrap_or(0),
            days: u32::try_from(sets[2]).unwrap_or(0),
            months: u16::try_from(sets[3]).unwrap_or(0),
            weekdays: u8::try_from(weekdays).unwrap_or(0),
            days_restricted: !fields[2].starts_with('*'),
            weekdays_restricted: !fields[4].starts_with('*'),
            text: fields.join(" "),
        })
    }

    /// The canonical text (lower case, single spaces).
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the expression fires on `date`.
    #[must_use]
    pub fn matches_date(&self, date: Date) -> bool {
        let month = u32::try_from(date.month()).unwrap_or(0);
        if self.months & (1 << month) == 0 {
            return false;
        }
        let day = u32::try_from(date.day()).unwrap_or(0);
        let day_ok = self.days & (1 << day) != 0;
        let weekday = match date.weekday() {
            Weekday::Sunday => 0,
            other => other.to_monday_one_offset(),
        };
        let weekday_ok = self.weekdays & (1 << weekday) != 0;
        if self.days_restricted && self.weekdays_restricted {
            day_ok || weekday_ok
        } else {
            day_ok && weekday_ok
        }
    }

    /// The (hour, minute) pairs it fires at on a matching day, in order.
    #[must_use]
    pub fn times(&self) -> Vec<(i8, i8)> {
        let mut times = Vec::new();
        for hour in 0..24_i8 {
            if self.hours & (1 << hour) == 0 {
                continue;
            }
            for minute in 0..60_i8 {
                if self.minutes & (1 << minute) != 0 {
                    times.push((hour, minute));
                }
            }
        }
        times
    }
}

#[cfg(test)]
mod tests {
    use super::CronExpr;
    use jiff::civil::date;

    #[test]
    fn fields_parse_ranges_steps_lists_and_names() {
        let expr = CronExpr::parse("*/15 9-17 * jan-mar MON-fri").unwrap();
        assert_eq!(expr.text(), "*/15 9-17 * jan-mar mon-fri");
        assert_eq!(expr.times().len(), 9 * 4);
        assert!(expr.matches_date(date(2026, 1, 5)));
        assert!(!expr.matches_date(date(2026, 1, 4)));
        assert!(!expr.matches_date(date(2026, 4, 6)));
        let sunday = CronExpr::parse("0 0 * * 7").unwrap();
        assert!(sunday.matches_date(date(2026, 1, 4)));
    }

    #[test]
    fn restricted_day_fields_match_either() {
        let expr = CronExpr::parse("0 0 1 * mon").unwrap();
        assert!(expr.matches_date(date(2026, 1, 1)));
        assert!(expr.matches_date(date(2026, 1, 5)));
        assert!(!expr.matches_date(date(2026, 1, 6)));
    }

    #[test]
    fn bad_fields_are_named() {
        assert_eq!(
            CronExpr::parse("60 * * * *").unwrap_err(),
            "minute: 60 is outside 0-59"
        );
        assert_eq!(
            CronExpr::parse("* * *").unwrap_err(),
            "expected 5 fields, found 3"
        );
        assert!(CronExpr::parse("*/0 * * * *").is_err());
        assert!(CronExpr::parse("5-1 * * * *").is_err());
    }
}
