//! Per-session automation rows (spec: session-automation, Persisted
//! per-session automation state).

use pmt_store::gateway::routes::{NewRoute, ResetReason};
use serde_json::json;

use crate::support::{TestHome, session};

#[test]
fn rows_are_kept_per_kind_across_reopening_and_go_with_the_session() {
    let home = TestHome::new();
    let s = {
        let store = home.store("default");
        let s = session(&store);
        store
            .put_automation(s.id, "goal", &json!({"text": "ship"}), "cli")
            .unwrap();
        store
            .put_automation(s.id, "heartbeat", &json!({"prompt": "ping"}), "gateway")
            .unwrap();
        store
            .put_automation(s.id, "goal", &json!({"text": "ship it"}), "tui")
            .unwrap();
        s
    };
    let store = home.store("default");
    let goal = store.automation(s.id, "goal").unwrap().unwrap();
    assert_eq!(goal.state["text"], "ship it");
    assert_eq!(goal.owner_surface, "tui");
    assert_eq!(store.automations_of(s.id).unwrap().len(), 2);
    assert_eq!(store.all_automation().unwrap().len(), 2);
    assert!(store.delete_automation(s.id, "goal").unwrap());
    assert!(!store.delete_automation(s.id, "goal").unwrap());
    store.delete_session(s.id, false).unwrap();
    assert!(store.all_automation().unwrap().is_empty());
}

#[test]
fn a_conversation_switching_session_drops_the_old_automation() {
    let home = TestHome::new();
    let store = home.store("default");
    let (old, new) = (session(&store), session(&store));
    let route = |id| NewRoute {
        key: "default:telegram:dm:7".into(),
        session_id: id,
        platform: "telegram".into(),
        chat_id: "7".into(),
        thread_id: None,
    };
    store.bind_route(&route(old.id), None).unwrap();
    store
        .put_automation(old.id, "heartbeat", &json!({"prompt": "ping"}), "gateway")
        .unwrap();
    store.bind_route(&route(old.id), None).unwrap();
    assert_eq!(store.automations_of(old.id).unwrap().len(), 1);
    store
        .bind_route(&route(new.id), Some(ResetReason::New))
        .unwrap();
    assert!(store.automations_of(old.id).unwrap().is_empty());
    // Resuming the archived conversation does not bring it back.
    store
        .bind_route(&route(old.id), Some(ResetReason::New))
        .unwrap();
    assert!(store.automations_of(old.id).unwrap().is_empty());
}

#[test]
fn a_stale_route_hands_the_automation_to_the_next_session() {
    let home = TestHome::new();
    let store = home.store("default");
    let (old, new) = (session(&store), session(&store));
    let route = |id| NewRoute {
        key: "default:telegram:dm:8".into(),
        session_id: id,
        platform: "telegram".into(),
        chat_id: "8".into(),
        thread_id: None,
    };
    store.bind_route(&route(old.id), None).unwrap();
    store
        .put_automation(old.id, "heartbeat", &json!({"prompt": "ping"}), "gateway")
        .unwrap();
    store
        .bind_route(&route(new.id), Some(ResetReason::StaleRoute))
        .unwrap();
    assert!(store.automations_of(old.id).unwrap().is_empty());
    let moved = store.automation(new.id, "heartbeat").unwrap().unwrap();
    assert_eq!(moved.state["prompt"], "ping");
}
