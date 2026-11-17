use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::{Rule, next_after};

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// The next `n` instants of `rule` started at `start`, in `zone`.
fn instants(rule: &str, start: &str, zone: &str, n: usize) -> Vec<String> {
    let zone = TimeZone::get(zone).unwrap();
    let rule = Rule::parse(rule).unwrap();
    let start = at(start);
    let mut base = start;
    let mut out = Vec::new();
    while out.len() < n {
        let Some(next) = next_after(&rule, start, base, &zone).unwrap() else {
            break;
        };
        out.push(
            next.to_zoned(zone.clone())
                .strftime("%a %Y-%m-%d %H:%M %Z")
                .to_string(),
        );
        base = next;
    }
    out
}

#[test]
fn rules_are_canonical_whatever_their_order_and_case() {
    let rule = Rule::parse("rrule:byminute=0;byday=th;freq=weekly;interval=2;byhour=12").unwrap();
    assert_eq!(
        rule.canonical(),
        "FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0"
    );
    let rule = Rule::parse("FREQ=MONTHLY;INTERVAL=1;BYDAY=-1FR,+1MO;WKST=SU;BYSECOND=0").unwrap();
    assert_eq!(rule.canonical(), "FREQ=MONTHLY;BYDAY=1MO,-1FR;WKST=SU");
}

#[test]
fn unsupported_or_invalid_rules_are_refused_with_a_reason() {
    for (text, why) in [
        ("BYDAY=MO", "needs FREQ"),
        ("FREQ=MINUTELY", "more often than once a minute"),
        ("FREQ=DAILY;COUNT=5;UNTIL=20270101", "cannot both"),
        ("FREQ=DAILY;COUNT=0", "out of range"),
        ("FREQ=DAILY;UNTIL=2027-01-01", "not a date"),
        ("FREQ=DAILY;UNTIL=20270231", "not a date"),
        ("FREQ=DAILY;UNTIL=20270101T250000Z", "not a date"),
        ("FREQ=DAILY;UNTIL=20270101T000000+01", "not a date"),
        ("FREQ=DAILY;BYHOUR=24", "out of range"),
        ("FREQ=WEEKLY;BYMONTHDAY=1", "WEEKLY"),
        ("FREQ=WEEKLY;BYDAY=1MO", "ordinals"),
        ("FREQ=YEARLY;BYDAY=1MO", "BYMONTH"),
        ("FREQ=DAILY;FREQ=WEEKLY", "twice"),
        ("FREQ=DAILY;BYEASTER=1", "unsupported"),
        ("FREQ=DAILY;BYSECOND=30", "finer"),
    ] {
        let err = Rule::parse(text).unwrap_err();
        assert!(err.contains(why), "{text}: {err}");
    }
}

#[test]
fn the_first_business_day_of_each_month() {
    // Starts Friday 13 November 2026; 1 January 2027 is a Friday and 1
    // May 2027 a Saturday.
    assert_eq!(
        instants(
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0",
            "2026-11-13T10:00:00Z",
            "Europe/Lisbon",
            4,
        ),
        [
            "Tue 2026-12-01 08:00 WET",
            "Fri 2027-01-01 08:00 WET",
            "Mon 2027-02-01 08:00 WET",
            "Mon 2027-03-01 08:00 WET",
        ]
    );
    assert_eq!(
        instants(
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0",
            "2027-04-15T10:00:00Z",
            "UTC",
            1,
        ),
        ["Mon 2027-05-03 08:00 UTC"]
    );
}

#[test]
fn ordinals_month_days_and_years() {
    assert_eq!(
        instants(
            "FREQ=MONTHLY;BYDAY=-1FR;BYHOUR=17;BYMINUTE=30",
            "2026-11-01T00:00:00Z",
            "UTC",
            2
        ),
        ["Fri 2026-11-27 17:30 UTC", "Fri 2026-12-25 17:30 UTC"]
    );
    assert_eq!(
        instants(
            "FREQ=MONTHLY;BYMONTHDAY=-1;BYHOUR=9;BYMINUTE=0",
            "2027-01-31T10:00:00Z",
            "UTC",
            2
        ),
        ["Sun 2027-02-28 09:00 UTC", "Wed 2027-03-31 09:00 UTC"]
    );
    assert_eq!(
        instants(
            "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29;BYHOUR=0;BYMINUTE=0",
            "2026-01-01T00:00:00Z",
            "UTC",
            2
        ),
        ["Tue 2028-02-29 00:00 UTC", "Sun 2032-02-29 00:00 UTC"]
    );
    assert_eq!(
        instants(
            "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH;BYHOUR=12;BYMINUTE=0",
            "2026-01-01T00:00:00Z",
            "UTC",
            2
        ),
        ["Thu 2026-11-26 12:00 UTC", "Thu 2027-11-25 12:00 UTC"]
    );
}

#[test]
fn hourly_and_daily_rules_filter_their_periods() {
    assert_eq!(
        instants(
            "FREQ=HOURLY;INTERVAL=3;BYHOUR=9,12,15;BYMINUTE=15",
            "2026-11-16T09:00:00Z",
            "UTC",
            4
        ),
        [
            "Mon 2026-11-16 09:15 UTC",
            "Mon 2026-11-16 12:15 UTC",
            "Mon 2026-11-16 15:15 UTC",
            "Tue 2026-11-17 09:15 UTC",
        ]
    );
    assert_eq!(
        instants(
            "FREQ=DAILY;INTERVAL=2;BYDAY=SA,SU;BYHOUR=10;BYMINUTE=0",
            "2026-11-13T00:00:00Z",
            "UTC",
            2
        ),
        ["Sun 2026-11-15 10:00 UTC", "Sat 2026-11-21 10:00 UTC"]
    );
}

#[test]
fn parts_left_out_come_from_the_start_and_wall_clock_survives_dst() {
    // Weekly on the start's weekday and time, in New York across the
    // November fall-back.
    assert_eq!(
        instants("FREQ=WEEKLY", "2026-10-29T13:30:00Z", "America/New_York", 2),
        ["Thu 2026-11-05 09:30 EST", "Thu 2026-11-12 09:30 EST"]
    );
}

#[test]
fn a_rule_that_matches_no_date_never_fires() {
    let rule = Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30").unwrap();
    let start = at("2026-01-01T00:00:00Z");
    assert_eq!(
        next_after(&rule, start, start, &TimeZone::UTC).unwrap(),
        None
    );
}

#[test]
fn until_and_count_keep_their_form_in_the_canonical_rule() {
    for (text, canonical) in [
        (
            "freq=daily;until=20271224;byhour=9",
            "FREQ=DAILY;BYHOUR=9;UNTIL=20271224",
        ),
        (
            "FREQ=DAILY;UNTIL=20271224T170000",
            "FREQ=DAILY;UNTIL=20271224T170000",
        ),
        (
            "FREQ=DAILY;UNTIL=20271224T170000Z",
            "FREQ=DAILY;UNTIL=20271224T170000Z",
        ),
        (
            "FREQ=WEEKLY;COUNT=3;BYDAY=MO",
            "FREQ=WEEKLY;BYDAY=MO;COUNT=3",
        ),
    ] {
        let rule = Rule::parse(text).unwrap();
        assert_eq!(rule.canonical(), canonical);
        assert_eq!(Rule::parse(canonical).unwrap(), rule, "round trip");
    }
}

#[test]
fn every_weekday_until_a_date_includes_that_whole_day() {
    // Starts Monday 21 December 2026; the 24th is a Thursday.
    assert_eq!(
        instants(
            "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=18;BYMINUTE=0;UNTIL=20261224",
            "2026-12-21T10:00:00Z",
            "Europe/Lisbon",
            10,
        ),
        [
            "Mon 2026-12-21 18:00 WET",
            "Tue 2026-12-22 18:00 WET",
            "Wed 2026-12-23 18:00 WET",
            "Thu 2026-12-24 18:00 WET",
        ]
    );
}

#[test]
fn a_local_until_is_wall_clock_and_a_utc_one_an_instant() {
    // 09:00 in New York is 14:00 UTC in December.
    let daily = |until: &str| {
        instants(
            &format!("FREQ=DAILY;BYHOUR=9;BYMINUTE=0;UNTIL={until}"),
            "2026-12-01T00:00:00Z",
            "America/New_York",
            10,
        )
        .len()
    };
    assert_eq!(daily("20261203T090000"), 3, "inclusive wall-clock bound");
    assert_eq!(daily("20261203T090000Z"), 2, "04:00 local on the 3rd");
    assert_eq!(daily("20261203T140000Z"), 3, "exactly the third instance");
}

#[test]
fn count_bounds_the_instances_from_the_start_whatever_the_base() {
    let rule = Rule::parse("FREQ=DAILY;BYHOUR=8;BYMINUTE=0;COUNT=3").unwrap();
    let start = at("2026-11-13T10:00:00Z");
    assert_eq!(
        instants(
            "FREQ=DAILY;BYHOUR=8;BYMINUTE=0;COUNT=3",
            "2026-11-13T10:00:00Z",
            "UTC",
            10,
        ),
        [
            "Sat 2026-11-14 08:00 UTC",
            "Sun 2026-11-15 08:00 UTC",
            "Mon 2026-11-16 08:00 UTC",
        ]
    );
    // Asked from later on (a restart, a missed run), the third is still
    // the last.
    let zone = TimeZone::UTC;
    assert_eq!(
        next_after(&rule, start, at("2026-11-15T12:00:00Z"), &zone).unwrap(),
        Some(at("2026-11-16T08:00:00Z"))
    );
    assert_eq!(
        next_after(&rule, start, at("2026-11-16T08:00:00Z"), &zone).unwrap(),
        None
    );
}
