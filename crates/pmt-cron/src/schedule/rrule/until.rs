//! `UNTIL`, the inclusive end of a rule, in the three RFC 5545 forms: a
//! date (`20271224`, through the end of that day on the zone's wall
//! clock), a floating date-time (`20271224T170000`, the zone's wall
//! clock) and a UTC date-time (`20271224T170000Z`, an absolute instant).

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;

/// The last moment a rule may fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Until {
    Date(Date),
    Local(DateTime),
    Utc(Timestamp),
}

/// The number in `text[range]`.
fn number(text: &str, range: std::ops::Range<usize>) -> Option<i16> {
    let part = text.get(range)?;
    part.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| part.parse().ok())?
}

fn date(text: &str) -> Option<Date> {
    let month = i8::try_from(number(text, 4..6)?).ok()?;
    let day = i8::try_from(number(text, 6..8)?).ok()?;
    Date::new(number(text, 0..4)?, month, day).ok()
}

/// The `HHMMSS` of a date-time's time part.
fn time(text: &str) -> Option<jiff::civil::Time> {
    let part = |r| number(text, r).and_then(|n| i8::try_from(n).ok());
    jiff::civil::Time::new(part(0..2)?, part(2..4)?, part(4..6)?, 0).ok()
}

impl Until {
    /// Parses an `UNTIL` value (upper case, as the rule parser gives it).
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let parsed = match value.len() {
            _ if !value.is_ascii() => None,
            8 => date(value).map(Self::Date),
            15 | 16 if value.as_bytes()[8] == b'T' => {
                let local = date(value)
                    .zip(time(&value[9..15]))
                    .map(|(d, t)| DateTime::from_parts(d, t));
                match (local, &value[15..]) {
                    (Some(local), "") => Some(Self::Local(local)),
                    (Some(local), "Z") => local
                        .to_zoned(TimeZone::UTC)
                        .ok()
                        .map(|z| Self::Utc(z.timestamp())),
                    _ => None,
                }
            }
            _ => None,
        };
        parsed.ok_or_else(|| {
            format!("UNTIL '{value}' is not a date (YYYYMMDD) or date-time (YYYYMMDDTHHMMSS[Z])")
        })
    }

    /// Whether the instance at wall time `wall` (instant `at`) comes after
    /// the bound.
    pub(crate) fn passed(&self, wall: DateTime, at: Timestamp) -> bool {
        match self {
            Self::Date(last) => wall.date() > *last,
            Self::Local(last) => wall > *last,
            Self::Utc(last) => at > *last,
        }
    }

    /// The value as RFC 5545 writes it.
    pub(crate) fn canonical(&self) -> String {
        match self {
            Self::Date(d) => d.strftime("%Y%m%d").to_string(),
            Self::Local(dt) => dt.strftime("%Y%m%dT%H%M%S").to_string(),
            Self::Utc(t) => t.strftime("%Y%m%dT%H%M%SZ").to_string(),
        }
    }
}
