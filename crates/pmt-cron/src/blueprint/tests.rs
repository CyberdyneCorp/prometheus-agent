use std::collections::BTreeMap;

use jiff::tz::TimeZone;
use serde_json::json;

use super::*;
use crate::schedule;

fn given(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn ready(fill: Fill) -> Filled {
    match fill {
        Fill::Ready(filled) => filled,
        Fill::Missing(slot) => panic!("slot {} is missing", slot.name),
    }
}

/// Scenario: Inline values skip the questions (the rendering half).
#[test]
fn inline_values_render_the_schedule_and_prompt() {
    let catalog = builtin::catalog();
    let brief = resolve(&catalog, "morning-brief").unwrap();
    let filled = ready(
        brief
            .fill(&given(&[("time", "08:00"), ("days", "weekdays")]))
            .unwrap(),
    );
    assert_eq!(filled.schedule, "0 8 * * 1-5");
    assert!(filled.prompt.starts_with("Prepare my morning brief"));
    let parsed =
        schedule::Schedule::parse(&filled.schedule, jiff::Timestamp::now(), &TimeZone::UTC)
            .unwrap();
    assert_eq!(
        format!("{} {}", parsed.kind(), parsed.canonical()),
        "cron 0 8 * * 1-5"
    );
}

/// Scenario: Invalid slot value.
#[test]
fn an_invalid_slot_value_is_refused_by_name() {
    let catalog = builtin::catalog();
    let brief = resolve(&catalog, "morning-brief").unwrap();
    let err = brief.fill(&given(&[("time", "25:00")])).unwrap_err();
    assert_eq!(err.code().code, codes::CRON_BLUEPRINT.code);
    assert!(
        err.message().starts_with("invalid value for slot 'time'"),
        "{}",
        err.message()
    );
    let unknown = brief.fill(&given(&[("hour", "8")])).unwrap_err();
    assert!(unknown.message().starts_with("unknown slot 'hour'"));
}

#[test]
fn a_required_slot_without_a_value_is_asked_for() {
    let catalog = builtin::catalog();
    let reminder = resolve(&catalog, "custom-reminder").unwrap();
    match reminder.fill(&BTreeMap::new()).unwrap() {
        Fill::Missing(slot) => assert_eq!(slot.name, "message"),
        Fill::Ready(_) => panic!("message has no default"),
    }
    let filled = ready(reminder.fill(&given(&[("message", "call mum")])).unwrap());
    assert_eq!(filled.prompt, "Remind me: call mum");
    assert_eq!(filled.schedule, "0 9 * * *");
}

#[test]
fn every_builtin_is_complete_and_its_defaults_parse() {
    let catalog = builtin::catalog();
    assert_eq!(catalog.len(), 16);
    for key in builtin::STARTERS {
        assert!(catalog.iter().any(|b| b.key == key), "{key}");
    }
    for blueprint in &catalog {
        blueprint.check().unwrap();
        let mut values = BTreeMap::new();
        for slot in blueprint.slots.iter().filter(|s| s.default.is_none()) {
            values.insert(slot.name.clone(), "something".to_owned());
        }
        let filled = ready(blueprint.fill(&values).unwrap());
        schedule::Schedule::parse(&filled.schedule, jiff::Timestamp::now(), &TimeZone::UTC)
            .unwrap_or_else(|e| panic!("{}: {e}", blueprint.key));
    }
}

#[test]
fn names_resolve_by_key_prefix_and_spelling() {
    let catalog = builtin::catalog();
    assert_eq!(resolve(&catalog, "price").unwrap().key, "price-watch");
    assert_eq!(
        resolve(&catalog, "morning-breif").unwrap().key,
        "morning-brief"
    );
    let ambiguous = resolve(&catalog, "h").unwrap_err();
    assert!(
        ambiguous.message().contains("ambiguous"),
        "{}",
        ambiguous.message()
    );
    assert!(resolve(&catalog, "zzzzzzzzzz").is_err());
}

#[test]
fn declared_blueprints_are_namespaced_and_checked() {
    let value = json!({
        "title": "Standup notes",
        "schedule": "{time.minute} {time.hour} * * 1-5",
        "prompt": "Collect standup notes for {team}",
        "slots": [
            {"name": "time", "kind": "time", "default": "09:15"},
            {"name": "team", "kind": "text"}
        ]
    });
    let origin = Origin::Plugin {
        id: "acme".to_owned(),
    };
    let blueprint = Blueprint::declared(value.clone(), "acme", "standup", origin).unwrap();
    assert_eq!(blueprint.key, "acme:standup");
    let catalog = [blueprint];
    assert_eq!(resolve(&catalog, "standup").unwrap().key, "acme:standup");
    let mut broken = value;
    broken["prompt"] = json!("Notes for {squad}");
    let err = Blueprint::declared(broken, "acme", "standup", Origin::Builtin).unwrap_err();
    assert!(err.message().contains("{squad}"), "{}", err.message());
}

#[test]
fn slot_value_words_are_name_value_pairs() {
    let words = vec!["time=08:00".to_owned(), "days=weekdays".to_owned()];
    assert_eq!(values(&words).unwrap().len(), 2);
    assert!(values(&["08:00".to_owned()]).is_err());
}
