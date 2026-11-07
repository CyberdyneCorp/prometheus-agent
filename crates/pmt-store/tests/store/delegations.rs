//! Durable delegation records (spec: subagent-delegation, Durable
//! background completions): a completion is claimed exactly once, put
//! back after a failed delivery, and dropped after 8 attempts or 48 hours.

use std::time::Duration;

use pmt_store::delegations::{Delivery, SubagentRow};

use super::support::{TestHome, session};

fn child(id: &str, session_id: &str) -> SubagentRow {
    SubagentRow {
        id: id.to_owned(),
        delegation_id: String::new(),
        task_index: 0,
        session_id: None,
        parent_session_id: session_id.to_owned(),
        root_session_id: session_id.to_owned(),
        depth: 1,
        goal: "g".to_owned(),
        status: "running".to_owned(),
        result: None,
    }
}

#[test]
fn a_completion_is_claimed_once_and_dropped_after_eight_attempts() {
    let home = TestHome::new();
    let store = home.store("default");
    let parent = session(&store);
    store
        .record_delegation(
            "dg-1",
            parent.id,
            Delivery::Pending,
            &[child("sa-1", &parent.id.to_string())],
        )
        .unwrap();
    assert!(
        store.pending_completions(parent.id).unwrap().is_empty(),
        "not complete yet"
    );
    assert!(store.subagent_finished("sa-1", "completed", "{}").unwrap());
    assert_eq!(store.pending_completions(parent.id).unwrap().len(), 1);
    assert!(store.claim_completion("dg-1").unwrap());
    assert!(
        !store.claim_completion("dg-1").unwrap(),
        "claimed exactly once"
    );
    for _ in 1..8 {
        assert!(store.release_completion("dg-1").unwrap());
        assert!(store.claim_completion("dg-1").unwrap());
    }
    assert!(
        !store.release_completion("dg-1").unwrap(),
        "the eighth failure drops it"
    );
    assert_eq!(
        store.delegation("dg-1").unwrap().unwrap().delivery,
        Delivery::Dropped
    );
}

#[test]
fn a_terminal_child_is_never_rewritten_and_old_completions_are_dropped() {
    let home = TestHome::new();
    let store = home.store("default");
    let parent = session(&store);
    store
        .record_delegation(
            "dg-2",
            parent.id,
            Delivery::Pending,
            &[child("sa-2", &parent.id.to_string())],
        )
        .unwrap();
    store
        .subagent_finished("sa-2", "completed", "{\"a\":1}")
        .unwrap();
    store
        .subagent_finished("sa-2", "failed", "{\"b\":2}")
        .unwrap();
    let row = &store.subagents_of("dg-2").unwrap()[0];
    assert_eq!(
        (row.status.as_str(), row.result.as_deref()),
        ("completed", Some("{\"a\":1}"))
    );
    store
        .age_delegation("dg-2", Duration::from_hours(49))
        .unwrap();
    assert_eq!(store.drop_stale_completions().unwrap(), ["dg-2"]);
}
