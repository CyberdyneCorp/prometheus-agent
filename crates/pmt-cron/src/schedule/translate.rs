//! Natural-language schedule translation (spec: scheduled-jobs,
//! Natural-language schedule translation): free-form text the grammar
//! rejects (`every other thursday at noon`) is translated by the
//! profile's auxiliary model, which the binary calls with
//! [`INSTRUCTIONS`] and [`prompt`]. The answer is never trusted: it must
//! parse with the deterministic grammar or as a supported RFC 5545 rule
//! ([`accept`]), and the caller always echoes it for confirmation.
//!
//! A translation is remembered for the confirmation window
//! ([`remember`], [`remembered`]), so the `cronjob` tool's confirming call
//! saves exactly what was echoed instead of asking the model again.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};

use super::Schedule;
use super::echo::zone_name;

/// The auxiliary model's instructions.
pub const INSTRUCTIONS: &str = "You translate a schedule written in plain language into \
    exactly one line, with nothing else. Prefer Prometheus' schedule grammar when it states \
    the schedule exactly: 'every <n><s|m|h|d|w>' (every 30m), a five-field cron expression \
    'minute hour day-of-month month day-of-week' (0 9 * * 1-5), 'tomorrow at 9am', 'in 2h' \
    or an RFC 3339 time. Otherwise answer an RFC 5545 rule 'RRULE:FREQ=...' using only \
    FREQ (HOURLY, DAILY, WEEKLY, MONTHLY or YEARLY), INTERVAL, BYMONTH, BYMONTHDAY, BYDAY \
    (ordinals such as 1MO or -1FR only in MONTHLY rules), BYSETPOS, BYHOUR, BYMINUTE and \
    WKST, always with BYHOUR and BYMINUTE, never COUNT or UNTIL. Examples: 'every other \
    thursday at noon' is RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0 and \
    'first business day of each month at 8' is \
    RRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0. Times are wall \
    clock times in the given time zone. If the text is not a schedule, answer UNTRANSLATABLE.";

/// The user message of a translation of `text` at `now` in `zone`.
#[must_use]
pub fn prompt(text: &str, now: Timestamp, zone: &TimeZone) -> String {
    let local = now.to_zoned(zone.clone());
    format!(
        "Time zone: {}\nNow: {}\nSchedule: {}",
        zone_name(zone),
        local.strftime("%A %Y-%m-%d %H:%M"),
        text.trim()
    )
}

/// The refusal of `text`: `could not translate schedule: <text>`.
#[must_use]
pub fn refused(text: &str) -> Error {
    Error::new(
        &codes::CRON_INVALID_SCHEDULE,
        format!("could not translate schedule: {}", text.trim()),
    )
}

/// The schedule the model's `reply` to `text` names, once it parses with
/// the grammar or as a supported recurrence rule (started at `now`).
///
/// # Errors
///
/// `cron.invalid_schedule`: `could not translate schedule: <text>`.
pub fn accept(reply: &str, text: &str, now: Timestamp, zone: &TimeZone) -> Result<Schedule, Error> {
    let line = reply
        .lines()
        .map(|l| l.trim().trim_matches(|c| c == '`' || c == '"' || c == '\''))
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.is_empty() || line.eq_ignore_ascii_case("UNTRANSLATABLE") {
        return Err(refused(text));
    }
    let parsed = if super::rrule::looks_like(line) {
        Schedule::rrule(line, now).map_err(|_| refused(text))
    } else {
        Schedule::parse(line, now, zone).map_err(|_| refused(text))
    };
    parsed.inspect_err(|_| tracing::warn!(reply = line, "schedule translation refused"))
}

/// A translation kept until its expiry (Unix ms), by text and zone.
type Kept = HashMap<(String, String), (Schedule, i64)>;

/// Translations kept for the confirmation round trip.
static KEPT: LazyLock<Mutex<Kept>> = LazyLock::new(Mutex::default);

fn key(text: &str, zone: &TimeZone) -> (String, String) {
    (
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
        zone_name(zone),
    )
}

/// Keeps the translation of `text` in `zone` for the confirmation window.
pub fn remember(text: &str, zone: &TimeZone, schedule: &Schedule, now: Timestamp) {
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    let now_ms = now.as_millisecond();
    kept.retain(|_, (_, expires)| *expires > now_ms);
    kept.insert(
        key(text, zone),
        (schedule.clone(), now_ms + crate::ops::CONFIRM_TTL_MS),
    );
}

/// The translation of `text` in `zone` made within the confirmation
/// window, if any.
#[must_use]
pub fn remembered(text: &str, zone: &TimeZone, now: Timestamp) -> Option<Schedule> {
    let kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    kept.get(&key(text, zone))
        .filter(|(_, expires)| *expires > now.as_millisecond())
        .map(|(schedule, _)| schedule.clone())
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::tz::TimeZone;

    use super::{accept, remember, remembered};
    use crate::schedule::{Echo, Schedule};

    fn now() -> Timestamp {
        // Friday 2026-11-13 10:00 UTC.
        "2026-11-13T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn a_biweekly_phrase_becomes_an_rrule_with_three_instants() {
        let zone = TimeZone::UTC;
        let schedule = accept(
            "RRULE:FREQ=WEEKLY;BYMINUTE=0;BYHOUR=12;INTERVAL=2;BYDAY=TH",
            "every other thursday at noon",
            now(),
            &zone,
        )
        .unwrap();
        let echo = Echo::of(&schedule, now(), now(), &zone);
        assert_eq!(
            echo.headline(),
            "rrule FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0 (UTC)"
        );
        let utc: Vec<&str> = echo.next.iter().map(|i| i.utc.as_str()).collect();
        // Weeks count from the start's (Monday 9 November, as RFC 5545
        // says): its Thursday has passed, so the first fire is on the
        // Thursday two weeks on.
        assert_eq!(
            utc,
            [
                "2026-11-26T12:00:00Z",
                "2026-12-10T12:00:00Z",
                "2026-12-24T12:00:00Z"
            ]
        );
    }

    #[test]
    fn grammar_answers_are_accepted_and_anything_else_refused() {
        let zone = TimeZone::UTC;
        assert_eq!(
            accept("`0 9 * * 1-5`\n", "weekday mornings", now(), &zone).unwrap(),
            Schedule::Cron {
                expr: "0 9 * * 1-5".to_owned()
            }
        );
        for reply in [
            "sometimes, when it rains",
            "UNTRANSLATABLE",
            "",
            "RRULE:FREQ=MINUTELY",
            "RRULE:FREQ=DAILY;COUNT=3",
            "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30",
        ] {
            let err = accept(reply, "whenever", now(), &zone).unwrap_err();
            assert_eq!(
                err.message(),
                "could not translate schedule: whenever",
                "{reply}"
            );
        }
    }

    #[test]
    fn a_translation_is_remembered_for_the_confirmation_window() {
        let zone = TimeZone::UTC;
        let schedule = Schedule::rrule("FREQ=DAILY;BYHOUR=8;BYMINUTE=0", now()).unwrap();
        remember("Each  Morning at 8", &zone, &schedule, now());
        assert_eq!(
            remembered("each morning at 8", &zone, now()),
            Some(schedule)
        );
        let later = now()
            .checked_add(jiff::SignedDuration::from_mins(11))
            .unwrap();
        assert_eq!(remembered("each morning at 8", &zone, later), None);
    }
}
