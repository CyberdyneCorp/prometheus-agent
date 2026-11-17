//! RFC 5545 recurrence rules (spec: scheduled-jobs, Natural-language
//! schedule translation): the validator and evaluator of the `rrule`
//! schedules a translation may produce. Supported parts: `FREQ` (`HOURLY`,
//! `DAILY`, `WEEKLY`, `MONTHLY`, `YEARLY`; nothing finer than the
//! scheduler's minute), `INTERVAL`, `BYMONTH`, `BYMONTHDAY`, `BYDAY` (with
//! ordinals in monthly rules and yearly rules with `BYMONTH`),
//! `BYSETPOS`, `BYHOUR`, `BYMINUTE`, `BYSECOND=0` and `WKST`. `COUNT` and
//! `UNTIL` are refused: a job's `repeat` bounds its runs. Parts a rule
//! leaves out take their value from its start (`DTSTART`), as RFC 5545
//! says; the start is the moment the job was created.

mod expand;

use std::collections::BTreeSet;
use std::fmt::Write as _;

pub(crate) use expand::next_after;

/// How often the rule repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Freq {
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    fn parse(text: &str) -> Result<Self, String> {
        Ok(match text {
            "HOURLY" => Self::Hourly,
            "DAILY" => Self::Daily,
            "WEEKLY" => Self::Weekly,
            "MONTHLY" => Self::Monthly,
            "YEARLY" => Self::Yearly,
            "SECONDLY" | "MINUTELY" => {
                return Err(format!("FREQ={text} repeats more often than once a minute"));
            }
            other => return Err(format!("unknown FREQ '{other}'")),
        })
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "HOURLY",
            Self::Daily => "DAILY",
            Self::Weekly => "WEEKLY",
            Self::Monthly => "MONTHLY",
            Self::Yearly => "YEARLY",
        }
    }
}

/// Weekday codes, Monday first (jiff's Monday-zero offset).
const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/// A `BYDAY` entry: the nth (negative: from the end) or every such day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ByDay {
    /// Monday = 0 (first, so canonical lists run Monday to Sunday).
    pub day: i8,
    pub nth: Option<i8>,
}

/// A validated rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub freq: Freq,
    pub interval: u32,
    pub by_month: BTreeSet<i8>,
    pub by_month_day: BTreeSet<i8>,
    pub by_day: BTreeSet<ByDay>,
    pub by_set_pos: BTreeSet<i16>,
    pub by_hour: BTreeSet<i8>,
    pub by_minute: BTreeSet<i8>,
    /// The first day of a week (Monday = 0): weekly intervals count
    /// whole weeks from it.
    pub week_start: i8,
}

fn numbers<T: std::str::FromStr + Ord>(
    name: &str,
    value: &str,
    valid: impl Fn(&T) -> bool,
) -> Result<BTreeSet<T>, String> {
    value
        .split(',')
        .map(|item| {
            item.parse::<T>()
                .ok()
                .filter(&valid)
                .ok_or_else(|| format!("{name} value '{item}' is out of range"))
        })
        .collect()
}

fn weekday(code: &str) -> Result<i8, String> {
    DAYS.iter()
        .position(|d| *d == code)
        .and_then(|p| i8::try_from(p).ok())
        .ok_or_else(|| format!("unknown weekday '{code}'"))
}

fn by_day(value: &str) -> Result<BTreeSet<ByDay>, String> {
    value
        .split(',')
        .map(|item| {
            let split = item.len().saturating_sub(2);
            let (nth, code) = item.split_at(split);
            let nth = match nth {
                "" => None,
                n => Some(
                    n.trim_start_matches('+')
                        .parse::<i8>()
                        .ok()
                        .filter(|n| *n != 0 && (-53..=53).contains(n))
                        .ok_or_else(|| format!("BYDAY value '{item}' is out of range"))?,
                ),
            };
            Ok(ByDay {
                nth,
                day: weekday(code)?,
            })
        })
        .collect()
}

impl Rule {
    /// Parses and validates `text` (`FREQ=WEEKLY;BYDAY=TH`, an optional
    /// `RRULE:` prefix, any case).
    ///
    /// # Errors
    ///
    /// Why the rule is invalid or outside the supported subset.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let upper = text.trim().to_ascii_uppercase();
        let body = upper.strip_prefix("RRULE:").unwrap_or(&upper);
        let mut seen = BTreeSet::new();
        let mut rule = Self {
            freq: Freq::Daily,
            interval: 1,
            by_month: BTreeSet::new(),
            by_month_day: BTreeSet::new(),
            by_day: BTreeSet::new(),
            by_set_pos: BTreeSet::new(),
            by_hour: BTreeSet::new(),
            by_minute: BTreeSet::new(),
            week_start: 0,
        };
        for part in body.split(';').filter(|p| !p.is_empty()) {
            let (name, value) = part
                .split_once('=')
                .ok_or_else(|| format!("'{part}' is not NAME=VALUE"))?;
            if !seen.insert(name.to_owned()) {
                return Err(format!("{name} is given twice"));
            }
            rule.set(name, value)?;
        }
        if !seen.contains("FREQ") {
            return Err("a rule needs FREQ".to_owned());
        }
        rule.check()?;
        Ok(rule)
    }

    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        match name {
            "FREQ" => self.freq = Freq::parse(value)?,
            "INTERVAL" => {
                self.interval = value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| format!("INTERVAL '{value}' is out of range"))?;
            }
            "BYMONTH" => self.by_month = numbers(name, value, |m: &i8| (1..=12).contains(m))?,
            "BYMONTHDAY" => {
                self.by_month_day =
                    numbers(name, value, |d: &i8| *d != 0 && (-31..=31).contains(d))?;
            }
            "BYDAY" => self.by_day = by_day(value)?,
            "BYSETPOS" => {
                self.by_set_pos =
                    numbers(name, value, |p: &i16| *p != 0 && (-366..=366).contains(p))?;
            }
            "BYHOUR" => self.by_hour = numbers(name, value, |h: &i8| (0..24).contains(h))?,
            "BYMINUTE" => self.by_minute = numbers(name, value, |m: &i8| (0..60).contains(m))?,
            "BYSECOND" if value == "0" => {}
            "BYSECOND" => return Err("BYSECOND other than 0 is finer than a minute".to_owned()),
            "WKST" => self.week_start = weekday(value)?,
            "COUNT" | "UNTIL" => {
                return Err(format!(
                    "{name} is not supported: bound the job's runs with its repeat instead"
                ));
            }
            other => return Err(format!("unsupported rule part '{other}'")),
        }
        Ok(())
    }

    /// The combinations RFC 5545 forbids or this evaluator does not
    /// support.
    fn check(&self) -> Result<(), String> {
        let ordinals = self.by_day.iter().any(|d| d.nth.is_some());
        match self.freq {
            Freq::Weekly if !self.by_month_day.is_empty() => {
                Err("BYMONTHDAY cannot be used with FREQ=WEEKLY".to_owned())
            }
            Freq::Hourly | Freq::Daily | Freq::Weekly if ordinals => Err(format!(
                "BYDAY ordinals need FREQ=MONTHLY or YEARLY, not {}",
                self.freq.as_str()
            )),
            Freq::Yearly if ordinals && self.by_month.is_empty() => {
                Err("BYDAY ordinals in a YEARLY rule need BYMONTH".to_owned())
            }
            _ => Ok(()),
        }
    }

    /// The canonical text: parts in a fixed order, `INTERVAL=1` and a
    /// Monday week start left out.
    pub(crate) fn canonical(&self) -> String {
        let mut text = format!("FREQ={}", self.freq.as_str());
        if self.interval != 1 {
            let _ = write!(text, ";INTERVAL={}", self.interval);
        }
        let list = |set: Vec<String>| set.join(",");
        let mut put = |name: &str, items: Vec<String>| {
            if !items.is_empty() {
                let _ = write!(text, ";{name}={}", list(items));
            }
        };
        put("BYMONTH", strings(&self.by_month));
        put("BYMONTHDAY", strings(&self.by_month_day));
        put(
            "BYDAY",
            self.by_day
                .iter()
                .map(|d| {
                    let code = DAYS[usize::try_from(d.day).unwrap_or(0)];
                    d.nth
                        .map_or_else(|| code.to_owned(), |n| format!("{n}{code}"))
                })
                .collect(),
        );
        put("BYSETPOS", strings(&self.by_set_pos));
        put("BYHOUR", strings(&self.by_hour));
        put("BYMINUTE", strings(&self.by_minute));
        if self.week_start != 0 {
            let _ = write!(
                text,
                ";WKST={}",
                DAYS[usize::try_from(self.week_start).unwrap_or(0)]
            );
        }
        text
    }
}

fn strings<T: ToString>(set: &BTreeSet<T>) -> Vec<String> {
    set.iter().map(ToString::to_string).collect()
}

/// Whether `text` looks like a recurrence rule rather than the grammar.
#[must_use]
pub(crate) fn looks_like(text: &str) -> bool {
    let upper = text.trim().to_ascii_uppercase();
    upper.starts_with("RRULE:") || upper.starts_with("FREQ=")
}

#[cfg(test)]
mod tests;
