//! The turn journal (spec: agent-turn-loop, Deterministic turn journal).

use pmt_store::{JournalKind as K, JournalRecord};

use crate::support::{TestHome, session};

#[test]
fn only_the_newest_turn_is_kept_and_an_unfinished_one_is_reported() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    store
        .journal(s, &JournalRecord::new("turn_a", K::TurnStarted))
        .unwrap();
    store
        .journal(
            s,
            &JournalRecord::new("turn_a", K::TurnFinished).with_data("completed"),
        )
        .unwrap();
    assert_eq!(store.unfinished_turn(s).unwrap(), None);

    store
        .journal(s, &JournalRecord::new("turn_b", K::TurnStarted))
        .unwrap();
    let mut started = JournalRecord::new("turn_b", K::ToolStarted).with_call("call_1");
    started.tool = Some("terminal".into());
    started.effect = Some("write".into());
    started.idempotent = Some(false);
    store.journal(s, &started).unwrap();

    // A crash here: reopened, the turn is still open.
    drop(store);
    let store = home.store("default");
    let open = store.unfinished_turn(s).unwrap().unwrap();
    let kinds: Vec<K> = open.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, [K::TurnStarted, K::ToolStarted]);
    assert_eq!(open[1], started);
    assert!(
        open.iter().all(|r| r.turn_id == "turn_b"),
        "turn_a was dropped"
    );
}

#[test]
fn deleting_the_session_deletes_its_journal() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    store
        .journal(s, &JournalRecord::new("turn_a", K::TurnStarted))
        .unwrap();
    store.delete_session(s, false).unwrap();
    assert!(store.journal_of(s).unwrap().is_empty());
}
