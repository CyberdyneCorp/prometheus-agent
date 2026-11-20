//! Achievements over a real session store (spec: pets, Achievements from
//! session history; Achievement unlocks celebrate): the incremental scan,
//! the per-session numbers and the unlock state.

use jiff::tz::{Offset, TimeZone};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_pets::achievements::{Profile, Unlock};
use pmt_store::{NewMessage, NewSession, Role, Source, Store};
use serde_json::json;

struct TestHome {
    _root: tempfile::TempDir,
    scope: ProfileScope,
}

fn home() -> TestHome {
    let root = tempfile::tempdir().unwrap();
    let home = Home::open(&root.path().join("home")).unwrap();
    let profile = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(
        profile,
        &Overrides::parse::<&str>(&[]).unwrap(),
        Launch::Other,
    )
    .unwrap();
    TestHome { _root: root, scope }
}

/// A time zone in which it is now 02:00-02:59: every session started
/// now is a night session.
fn night() -> TimeZone {
    let utc = jiff::Timestamp::now().to_zoned(TimeZone::UTC);
    let hours = (2 - i32::from(utc.hour())).rem_euclid(24);
    let hours = if hours > 12 { hours - 24 } else { hours };
    TimeZone::fixed(Offset::from_hours(i8::try_from(hours).unwrap()).unwrap())
}

fn sessions(store: &Store, n: usize) {
    for _ in 0..n {
        store.create_session(&NewSession::new(Source::Cli)).unwrap();
    }
}

/// Scenario: Incremental rescan.
#[test]
fn only_new_or_changed_sessions_are_analysed() {
    let home = home();
    let store = Store::open(&home.scope).unwrap();
    sessions(&store, 2000);
    let achievements = Profile::new(home.scope.dir(), TimeZone::UTC);
    let first = achievements.scan(&store).unwrap();
    assert_eq!((first.analysed, first.reused), (2000, 0));
    assert!(first.first);
    sessions(&store, 3);
    let second = achievements.scan(&store).unwrap();
    assert_eq!((second.analysed, second.reused), (3, 2000));
    assert!(!second.first);
    // A session with a new message is analysed again; nothing else.
    let some = store
        .list_sessions(&pmt_store::ListOptions::default())
        .unwrap()[10]
        .id;
    std::thread::sleep(std::time::Duration::from_millis(5));
    store
        .append_message(some, &NewMessage::new(Role::User, "more"))
        .unwrap();
    let third = achievements.scan(&store).unwrap();
    assert_eq!((third.analysed, third.reused), (1, 2002));
    assert_eq!(achievements.snapshot().sessions, 2003);
}

fn assistant_calls(names: &[&str]) -> NewMessage {
    let mut row = NewMessage::new(Role::Assistant, "");
    let calls: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| json!({"id": format!("c{i}"), "name": n, "arguments": {}}))
        .collect();
    row.tool_calls = Some(json!(calls));
    row
}

fn tool_result(name: &str, error: bool) -> NewMessage {
    let mut row = NewMessage::new(Role::Tool, if error { "failed" } else { "ok" });
    row.tool_name = Some(name.to_owned());
    row.tool_call_id = Some("c0".to_owned());
    row.metadata = Some(json!({"status": if error { "error" } else { "ok" }, "is_error": error}));
    row
}

#[test]
fn badges_count_chains_recoveries_and_night_sessions() {
    let home = home();
    let store = Store::open(&home.scope).unwrap();
    let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
    let names = [
        "terminal",
        "read_file",
        "terminal",
        "memory",
        "skill_view",
        "web_search",
        "browser_navigate",
        "delegate_task",
        "patch",
        "terminal",
    ];
    for row in [
        NewMessage::new(Role::User, "go"),
        assistant_calls(&names),
        tool_result("terminal", true),
        tool_result("terminal", true),
        tool_result("terminal", false),
        NewMessage::new(Role::User, "again"),
        assistant_calls(&["terminal"]),
        tool_result("terminal", false),
    ] {
        store.append_message(session.id, &row).unwrap();
    }
    let achievements = Profile::new(home.scope.dir(), night());
    achievements.scan(&store).unwrap();
    let snapshot = achievements.snapshot();
    let value = |id: &str| snapshot.badges.iter().find(|b| b.id == id).unwrap().value;
    assert_eq!(value("chain-reaction"), 10, "the most calls in one turn");
    assert_eq!(value("busy-hands"), 11);
    assert_eq!(value("toolsmith"), 8);
    assert_eq!(value("red-text"), 2);
    assert_eq!(
        value("bug-squasher"),
        2,
        "both failures were fixed by a later success"
    );
    assert_eq!(value("elephant"), 1);
    assert_eq!(value("skillful"), 1);
    assert_eq!(value("researcher"), 2);
    assert_eq!(value("delegator"), 1);
    assert_eq!(value("night-owl"), 1);
    assert_eq!(value("marathon"), 4);
    let chain = snapshot
        .badges
        .iter()
        .find(|b| b.id == "chain-reaction")
        .unwrap();
    assert_eq!(chain.tier.as_deref(), Some("copper"));
    assert_eq!(chain.next_tier.as_deref(), Some("silver"));
    assert!(snapshot.badges.iter().all(|b| !b.description.is_empty()));
}

#[test]
fn unlocks_persist_and_only_new_tiers_are_reported() {
    let home = home();
    let store = Store::open(&home.scope).unwrap();
    sessions(&store, 9);
    let achievements = Profile::new(home.scope.dir(), night());
    let first = achievements.scan(&store).unwrap();
    assert!(
        first.first,
        "a profile never scanned records its past unlocks"
    );
    assert!(first.unlocked.contains(&Unlock {
        id: "night-owl".into(),
        tier: "copper".into()
    }));
    assert!(home.scope.dir().join("achievements.json").is_file());
    assert!(achievements.scan(&store).unwrap().unlocked.is_empty());
    sessions(&store, 1);
    let tenth = achievements.scan(&store).unwrap();
    assert!(
        tenth.unlocked.contains(&Unlock {
            id: "night-owl".into(),
            tier: "silver".into()
        }),
        "{tenth:?}"
    );
    assert!(
        tenth.unlocked.contains(&Unlock {
            id: "regular".into(),
            tier: "copper".into()
        }),
        "{tenth:?}"
    );
    let owl = achievements
        .snapshot()
        .badges
        .into_iter()
        .find(|b| b.id == "night-owl")
        .unwrap();
    assert_eq!(owl.tier.as_deref(), Some("silver"));
    assert!(owl.unlocked_at.is_some());
}
