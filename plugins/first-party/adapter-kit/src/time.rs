//! Wall-clock time for plugins, which the sandbox does not offer: every
//! answer [`crate::net::send`] receives carries the platform's `Date`
//! header, and the latest one plus the polls counted since ([`crate::clock`])
//! is "now". Token expiries, subscription renewals and signed assertions
//! need it; until a first answer arrived it is unknown.

use std::sync::Mutex;

use crate::clock;

/// The latest `Date` seen: (epoch seconds, plugin clock ms then).
static SEEN: Mutex<Option<(u64, u64)>> = Mutex::new(None);

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Records a `Date` header value (an IMF-fixdate).
pub fn observe(date: &str) {
    if let Some(secs) = parse_http_date(date)
        && let Ok(mut seen) = SEEN.lock()
    {
        *seen = Some((secs, clock::now_ms()));
    }
}

/// Seconds since the epoch, when a `Date` header was seen.
#[must_use]
pub fn now() -> Option<u64> {
    let (secs, at) = (*SEEN.lock().ok()?)?;
    Some(secs + clock::now_ms().saturating_sub(at) / 1000)
}

/// Days since 1970-01-01 of a civil date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The civil date of days since 1970-01-01 (`civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn seconds(days: i64, h: i64, m: i64, s: i64) -> Option<u64> {
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

/// Seconds since the epoch of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
#[must_use]
pub fn parse_http_date(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace().skip(1);
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == name)?).ok()? + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    seconds(days_from_civil(year, month, day), h, m, s)
}

/// Seconds since the epoch of an ISO 8601 UTC time
/// (`2026-11-22T10:00:00Z`, fractions and `+00:00` accepted).
#[must_use]
pub fn parse_iso(text: &str) -> Option<u64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut ymd = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let clock_part = rest
        .trim_end_matches('Z')
        .trim_end_matches("+00:00")
        .split('.')
        .next()?;
    let mut hms = clock_part.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (
        hms.next()??,
        hms.next()??,
        hms.next().flatten().unwrap_or(0),
    );
    seconds(days_from_civil(year, month, day), h, m, s)
}

/// ISO 8601 UTC of `secs` since the epoch (`2026-11-22T10:00:00Z`).
#[must_use]
pub fn iso(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or_default();
    let rest = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        let secs = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(secs, 784_111_777);
        assert_eq!(iso(secs), "1994-11-06T08:49:37Z");
        assert_eq!(parse_iso("1994-11-06T08:49:37.1234567Z"), Some(secs));
        assert_eq!(parse_iso("2024-02-29T00:00:00+00:00"), Some(1_709_164_800));
        assert_eq!(iso(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn now_follows_the_last_date_seen() {
        assert_eq!(now(), None);
        observe("Sun, 06 Nov 1994 08:49:37 GMT");
        for _ in 0..8 {
            clock::tick();
        }
        // Other tests may tick the shared clock too.
        assert!(now().unwrap() >= 784_111_779);
    }
}
