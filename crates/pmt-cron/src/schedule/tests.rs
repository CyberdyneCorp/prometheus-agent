//! Grammar table and next-run tests (spec: scheduled-jobs, Unambiguous
//! schedule grammar; Next-run computation; Canonical schedule echo).

use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::{Echo, Schedule, at_ms};

fn now() -> Timestamp {
    "2026-11-04T10:00:00Z".parse().unwrap()
}

fn parse(input: &str) -> Result<Schedule, String> {
    Schedule::parse(input, now(), &TimeZone::UTC).map_err(|e| e.message().to_owned())
}

/// Scenario: Bare duration is rejected, not guessed.
#[test]
fn a_bare_duration_is_rejected_naming_both_readings() {
    assert_eq!(
        parse("30m").unwrap_err(),
        "ambiguous schedule '30m': use 'every 30m' for a recurring interval or 'in 30m' for a one-shot"
    );
}

/// Scenario: Explicit forms parse to their kinds.
#[test]
fn explicit_forms_parse_to_their_kinds() {
    assert_eq!(
        parse("every 30m").unwrap(),
        Schedule::Interval { every_secs: 1800 }
    );
    let once = parse("in 30m").unwrap();
    assert_eq!(once.kind(), "once");
    assert_eq!(
        once,
        Schedule::Once {
            at_ms: now().as_millisecond() + 30 * 60 * 1000
        }
    );
    assert_eq!(
        parse("0 9 * * 1-5").unwrap(),
        Schedule::Cron {
            expr: "0 9 * * 1-5".to_owned()
        }
    );
}

/// Scenario: Sub-minute interval rejected.
#[test]
fn a_sub_minute_interval_is_rejected() {
    assert_eq!(
        parse("every 10s").unwrap_err(),
        "interval must be at least 60s"
    );
    assert_eq!(parse("in 59s").unwrap_err(), "delay must be at least 60s");
    assert!(parse("every 60s").is_ok());
}

#[test]
fn grammar_table() {
    let cases: &[(&str, &str, &str)] = &[
        ("every 2h", "interval", "every 2h"),
        ("every 90s", "interval", "every 90s"),
        ("every monday 9am", "cron", "0 9 * * 1"),
        ("weekdays at 9:30", "cron", "30 9 * * 1-5"),
        ("every day at 18:00", "cron", "0 18 * * *"),
        ("*/15  *  * * *", "cron", "*/15 * * * *"),
        ("2026-11-05T08:00:00Z", "once", "2026-11-05T08:00:00Z"),
        ("at 9am", "once", "2026-11-05T09:00:00Z"),
        ("at 11:00", "once", "2026-11-04T11:00:00Z"),
        ("tomorrow at 18:00", "once", "2026-11-05T18:00:00Z"),
        ("at 2026-12-01 07:30", "once", "2026-12-01T07:30:00Z"),
        ("in 2h", "once", "2026-11-04T12:00:00Z"),
    ];
    for (input, kind, canonical) in cases {
        let schedule = parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(schedule.kind(), *kind, "{input}");
        assert_eq!(schedule.canonical(), *canonical, "{input}");
        // The canonical text parses back to the same schedule.
        if *kind != "once" {
            assert_eq!(parse(canonical).unwrap(), schedule, "{input}");
        }
    }
    for bad in [
        "",
        "sometimes",
        "every",
        "every 0m",
        "in soon",
        "0 9 * *",
        "61 * * * *",
        "every blursday at 9am",
    ] {
        let err = parse(bad).unwrap_err();
        assert!(
            err.starts_with("invalid schedule") || err.starts_with("invalid cron expression"),
            "{bad}: {err}"
        );
    }
    assert!(parse("invalid").unwrap_err().contains("'every <duration>'"));
    assert!(
        parse("2020-01-01T00:00:00Z")
            .unwrap_err()
            .contains("in the past")
    );
    assert!(parse("0 0 30 2 *").unwrap_err().contains("never fires"));
}

/// Scenario: DST fall-back fires once.
#[test]
fn a_fall_back_time_fires_once_at_its_first_occurrence() {
    let zone = TimeZone::get("America/New_York").unwrap();
    let schedule = parse("every day at 1:30").unwrap();
    // 2026-11-01 is the fall-back date: 01:30 happens twice.
    let day_start: Timestamp = "2026-11-01T04:00:00Z".parse().unwrap();
    let day_end: Timestamp = "2026-11-02T05:00:00Z".parse().unwrap();
    let mut fired = Vec::new();
    let mut base = day_start;
    while let Some(at) = schedule.next_after(base, base, &zone).unwrap() {
        if at >= day_end {
            break;
        }
        fired.push(at);
        base = at;
    }
    let first: Timestamp = "2026-11-01T05:30:00Z".parse().unwrap();
    assert_eq!(fired, [first]);
}

#[test]
fn a_spring_forward_time_fires_at_the_first_valid_instant_after_it() {
    let zone = TimeZone::get("America/New_York").unwrap();
    let schedule = parse("every day at 2:30").unwrap();
    let base: Timestamp = "2026-03-08T05:00:00Z".parse().unwrap();
    let at = schedule.next_after(base, base, &zone).unwrap().unwrap();
    // 02:00 EST jumps to 03:00 EDT at 07:00 UTC.
    assert_eq!(at, "2026-03-08T07:00:00Z".parse::<Timestamp>().unwrap());
}

/// Scenario: Interval does not drift with run length.
#[test]
fn an_interval_counts_from_the_previous_scheduled_instant() {
    let schedule = parse("every 1h").unwrap();
    let scheduled: Timestamp = "2026-11-04T10:00:00Z".parse().unwrap();
    let finished: Timestamp = "2026-11-04T10:07:00Z".parse().unwrap();
    let next = schedule
        .next_after(finished, scheduled, &TimeZone::UTC)
        .unwrap()
        .unwrap();
    assert_eq!(next, "2026-11-04T11:00:00Z".parse::<Timestamp>().unwrap());
    // Strictly after the base.
    let next = schedule
        .next_after(scheduled, scheduled, &TimeZone::UTC)
        .unwrap()
        .unwrap();
    assert_eq!(next, "2026-11-04T11:00:00Z".parse::<Timestamp>().unwrap());
}

#[test]
fn the_echo_shows_the_zone_and_the_next_three_instants() {
    let zone = TimeZone::get("Europe/Lisbon").unwrap();
    let schedule = Schedule::parse("weekdays at 9am", now(), &zone).unwrap();
    let echo = Echo::of(&schedule, now(), now(), &zone);
    assert_eq!(echo.headline(), "cron 0 9 * * 1-5 (Europe/Lisbon)");
    let utc: Vec<&str> = echo.next.iter().map(|i| i.utc.as_str()).collect();
    assert_eq!(
        utc,
        [
            "2026-11-05T09:00:00Z",
            "2026-11-06T09:00:00Z",
            "2026-11-09T09:00:00Z"
        ]
    );
    assert!(
        echo.render().contains("next: Mon 2026-11-09 09:00 WET"),
        "{}",
        echo.render()
    );
    let once = Schedule::Once {
        at_ms: now().as_millisecond() + 3_600_000,
    };
    assert_eq!(Echo::of(&once, now(), now(), &zone).next.len(), 1);
    assert_eq!(at_ms(0), Timestamp::UNIX_EPOCH);
}
