//! The suggested automations inbox (spec: scheduled-jobs, Suggested
//! automations inbox).

mod support;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_cron::suggest::{Source, Suggestion, dismiss, numbered, offer, offer_catalog, pending};
use support::TestProfile;

fn usage(key: &str) -> Suggestion {
    Suggestion {
        key: key.to_owned(),
        source: Source::Usage,
        title: "Weekly standup".to_owned(),
        schedule: "0 9 * * 1".to_owned(),
        prompt: "Collect my standup notes".to_owned(),
    }
}

/// Scenario: Dismissed suggestion is not re-offered.
#[test]
fn a_dismissed_suggestion_is_not_offered_again() {
    let profile = TestProfile::new();
    let store = profile.store();
    assert!(offer(&store, &usage("weekly-standup")).unwrap());
    let row = dismiss(&store, 1).unwrap();
    assert_eq!(row.key, "weekly-standup");
    assert!(!offer(&store, &usage("weekly-standup")).unwrap());
    assert!(pending(&store).unwrap().is_empty());
}

#[test]
fn at_most_five_wait_and_the_starters_come_from_the_catalog() {
    let profile = TestProfile::new();
    let store = profile.store();
    assert_eq!(offer_catalog(&store).unwrap(), 4);
    assert_eq!(
        offer_catalog(&store).unwrap(),
        0,
        "known keys are not offered twice"
    );
    assert!(offer(&store, &usage("a")).unwrap());
    assert!(!offer(&store, &usage("b")).unwrap(), "the sixth is dropped");
    let rows = pending(&store).unwrap();
    assert_eq!(rows.len(), 5);
    let starters: Vec<_> = rows.iter().filter(|r| r.source == "catalog").collect();
    assert_eq!(starters.len(), 4);
    assert!(starters.iter().all(|r| !r.schedule.is_empty()));
    assert!(numbered(&store, 6).is_err());
}

#[test]
fn a_proposed_suggestion_is_checked() {
    let now = Timestamp::now();
    assert!(usage("weekly-standup").check(now, &TimeZone::UTC).is_ok());
    assert!(usage("Weekly Standup").check(now, &TimeZone::UTC).is_err());
    let mut bad = usage("ok");
    bad.schedule = "5m".to_owned();
    assert!(bad.check(now, &TimeZone::UTC).is_err());
}
