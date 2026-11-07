//! The canonical schedule echo (spec: scheduled-jobs, Canonical schedule
//! echo and confirmation): kind, canonical text, resolved zone and the
//! next three fire instants in local time and UTC, shown before any
//! create or schedule edit is saved.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use super::Schedule;

/// One upcoming fire instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Instant {
    /// Local wall-clock time with the zone abbreviation.
    pub local: String,
    /// RFC 3339 UTC.
    pub utc: String,
}

/// What is shown before saving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Echo {
    /// `interval`, `cron` or `once`.
    pub kind: String,
    /// The canonical schedule text.
    pub canonical: String,
    /// The IANA zone (or `UTC`, `host`).
    pub timezone: String,
    /// Up to three next instants.
    pub next: Vec<Instant>,
}

/// The display name of `zone`.
#[must_use]
pub(super) fn zone_name(zone: &TimeZone) -> String {
    zone.iana_name()
        .map_or_else(|| "host".to_owned(), str::to_owned)
}

impl Echo {
    /// The echo of `schedule` from `now` in `zone`, with an interval
    /// counted from `anchor`.
    #[must_use]
    pub fn of(schedule: &Schedule, now: Timestamp, anchor: Timestamp, zone: &TimeZone) -> Self {
        let mut next = Vec::new();
        let mut base = now;
        let mut anchor = anchor;
        while next.len() < 3 {
            let Ok(Some(at)) = schedule.next_after(base, anchor, zone) else {
                break;
            };
            let local = at.to_zoned(zone.clone());
            next.push(Instant {
                local: local.strftime("%a %Y-%m-%d %H:%M %Z").to_string(),
                utc: at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            });
            base = at;
            anchor = at;
        }
        Self {
            kind: schedule.kind().to_owned(),
            canonical: schedule.canonical(),
            timezone: zone_name(zone),
            next,
        }
    }

    /// The first line: `cron 0 9 * * 1-5 (Europe/Lisbon)`.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("{} {} ({})", self.kind, self.canonical, self.timezone)
    }

    /// The headline and one `next:` line per instant.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut text = self.headline();
        for instant in &self.next {
            let _ = write!(text, "\n  next: {} ({})", instant.local, instant.utc);
        }
        text
    }
}
