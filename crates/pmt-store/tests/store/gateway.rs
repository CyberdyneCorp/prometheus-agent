//! The messaging gateway's tables (spec: messaging-gateway, Conversations
//! never rotate on time; Documented pairing flow; Durable delivery outbox;
//! Stuck-loop suspension).

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::codes;
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind, OutboundState};
use pmt_store::gateway::pairing::{Requested, Requester};
use pmt_store::gateway::routes::{NewRoute, ResetReason};

use crate::support::{TestHome, session};

fn route(key: &str, session: pmt_core::id::SessionId) -> NewRoute {
    NewRoute {
        key: key.to_owned(),
        session_id: session,
        platform: "telegram".into(),
        chat_id: "123".into(),
        thread_id: None,
    }
}

#[test]
fn a_pruned_route_keeps_its_session_searchable() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    store
        .bind_route(&route("default:telegram:dm:123", s.id), None)
        .unwrap();
    let old = Timestamp::now() - SignedDuration::from_hours(24 * 120);
    store.touch_route("default:telegram:dm:123", old).unwrap();
    let cutoff = Timestamp::now() - SignedDuration::from_hours(24 * 90);
    assert_eq!(store.prune_routes(cutoff).unwrap(), 1);
    assert_eq!(store.route("default:telegram:dm:123").unwrap(), None);
    assert!(store.session(s.id).is_ok(), "the session survives pruning");
}

#[test]
fn a_replacing_session_records_why() {
    let home = TestHome::new();
    let store = home.store("default");
    let first = session(&store);
    let second = session(&store);
    let key = "default:telegram:dm:123";
    store.bind_route(&route(key, first.id), None).unwrap();
    store
        .bind_route(&route(key, second.id), Some(ResetReason::Suspended))
        .unwrap();
    assert_eq!(store.route(key).unwrap().unwrap().session_id, second.id);
    let state = store.routed_session(second.id).unwrap().unwrap();
    assert_eq!(state.reset_reason, Some(ResetReason::Suspended));
    assert_eq!(
        store
            .routed_session(first.id)
            .unwrap()
            .unwrap()
            .reset_reason,
        None
    );
}

#[test]
fn the_third_restart_during_a_turn_suspends_the_session() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    let key = "default:telegram:dm:123";
    store.bind_route(&route(key, s.id), None).unwrap();
    for restart in 1..=3u32 {
        store.set_turn_active(s.id, true).unwrap();
        let found = store.recover_interrupted(3).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].suspended, restart == 3, "restart {restart}");
        assert_eq!(found[0].route.as_ref().unwrap().key, key);
        let state = store.routed_session(s.id).unwrap().unwrap();
        assert!(state.resume_pending);
        assert_eq!(state.restart_count, restart);
    }
    assert!(
        store.recover_interrupted(3).unwrap().is_empty(),
        "a recovery reports each interrupted turn once"
    );
}

#[test]
fn a_completed_turn_resets_the_restart_counter() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    store
        .bind_route(&route("default:telegram:dm:1", s.id), None)
        .unwrap();
    store.set_turn_active(s.id, true).unwrap();
    store.recover_interrupted(3).unwrap();
    store.set_turn_active(s.id, true).unwrap();
    store.set_turn_active(s.id, false).unwrap();
    let state = store.routed_session(s.id).unwrap().unwrap();
    assert_eq!(state.restart_count, 0);
    assert!(!state.resume_pending);
}

fn requester(sender: &str) -> Requester<'_> {
    Requester {
        platform: "telegram",
        sender_id: sender,
        chat_id: sender,
        sender_name: Some("Ann"),
    }
}

#[test]
fn a_code_approves_its_sender_whatever_the_formatting() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::now();
    let Requested::Issued(code) = store.request_pairing(&requester("42"), now).unwrap() else {
        panic!("no code issued");
    };
    assert!(!store.is_paired("telegram", "42").unwrap());
    let typed = format!("{} {}", &code[..4], &code[4..]).to_lowercase();
    let approved = store.approve_pairing("telegram", &typed, now).unwrap();
    assert_eq!(approved.sender_id, "42");
    assert!(approved.approved);
    assert!(store.is_paired("telegram", "42").unwrap());
    assert!(
        !store.is_paired("discord", "42").unwrap(),
        "pairing is per platform"
    );
    let text = std::fs::read(home.database("default")).unwrap();
    let plain = String::from_utf8_lossy(&text);
    assert!(!plain.contains(&code), "the plaintext code is never stored");
}

#[test]
fn requests_are_rate_limited_per_sender_and_platform() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::now();
    assert!(matches!(
        store.request_pairing(&requester("1"), now).unwrap(),
        Requested::Issued(_)
    ));
    assert_eq!(
        store.request_pairing(&requester("1"), now).unwrap(),
        Requested::TooSoon
    );
    store.request_pairing(&requester("2"), now).unwrap();
    store.request_pairing(&requester("3"), now).unwrap();
    assert_eq!(
        store.request_pairing(&requester("4"), now).unwrap(),
        Requested::TooManyPending
    );
    let later = now + SignedDuration::from_mins(61);
    assert!(
        matches!(
            store.request_pairing(&requester("4"), later).unwrap(),
            Requested::Issued(_)
        ),
        "expired codes free their slots"
    );
}

#[test]
fn five_failures_lock_pairing_even_for_a_valid_code() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::now();
    let Requested::Issued(code) = store.request_pairing(&requester("42"), now).unwrap() else {
        panic!("no code issued");
    };
    for _ in 0..5 {
        let err = store
            .approve_pairing("telegram", "ZZZZZZZZ", now)
            .unwrap_err();
        assert_eq!(err.code(), &codes::GATEWAY_PAIRING_UNKNOWN);
    }
    let err = store.approve_pairing("telegram", &code, now).unwrap_err();
    assert_eq!(err.code(), &codes::GATEWAY_PAIRING_LOCKED);
    assert!(
        err.message()
            .starts_with("pairing locked for telegram until "),
        "{}",
        err.message()
    );
    assert!(!store.is_paired("telegram", "42").unwrap());
    assert!(
        store
            .approve_pairing("discord", "ZZZZZZZZ", now)
            .unwrap_err()
            .code()
            == &codes::GATEWAY_PAIRING_UNKNOWN,
        "the lock is per platform"
    );
}

#[test]
fn an_approved_sender_can_be_revoked() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::now();
    let Requested::Issued(code) = store.request_pairing(&requester("42"), now).unwrap() else {
        panic!("no code issued");
    };
    store.approve_pairing("telegram", &code, now).unwrap();
    assert_eq!(store.pairings(None).unwrap().len(), 1);
    store.revoke_pairing("telegram", "42").unwrap();
    assert!(!store.is_paired("telegram", "42").unwrap());
    assert_eq!(
        store.revoke_pairing("telegram", "42").unwrap_err().code(),
        &codes::GATEWAY_PAIRING_NOT_FOUND
    );
}

fn outbound(key: &str, chat: &str) -> NewOutbound {
    NewOutbound {
        idempotency_key: key.to_owned(),
        session_id: None,
        platform: "discord".into(),
        chat_id: chat.into(),
        thread_id: None,
        reply_to: None,
        edit_of: None,
        kind: OutboundKind::Reply,
        body: format!("body of {key}"),
        attachments: Vec::new(),
        notify: None,
    }
}

#[test]
fn an_entry_is_stored_once_per_idempotency_key() {
    let home = TestHome::new();
    let store = home.store("default");
    let (first, fresh) = store.enqueue_outbound(&outbound("s:t:0", "c")).unwrap();
    assert!(fresh);
    let (again, fresh) = store.enqueue_outbound(&outbound("s:t:0", "c")).unwrap();
    assert!(!fresh);
    assert_eq!(first.id, again.id);
}

#[test]
fn parts_of_one_chat_leave_in_order() {
    let home = TestHome::new();
    let store = home.store("default");
    let (a, _) = store.enqueue_outbound(&outbound("s:t:0", "c")).unwrap();
    let (b, _) = store.enqueue_outbound(&outbound("s:t:1", "c")).unwrap();
    let (other, _) = store.enqueue_outbound(&outbound("s:u:0", "d")).unwrap();
    let now = Timestamp::now();
    let due: Vec<i64> = store
        .due_outbound(now, 10)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(due, vec![a.id, other.id], "b waits behind a");
    assert!(store.start_sending(a.id).unwrap());
    store
        .retry_later(a.id, now + SignedDuration::from_secs(60), false, "503")
        .unwrap();
    let due: Vec<i64> = store
        .due_outbound(now, 10)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(due, vec![other.id], "a retrying head still holds b back");
    let later = now + SignedDuration::from_secs(61);
    assert_eq!(store.due_outbound(later, 10).unwrap()[0].id, a.id);
    store.start_sending(a.id).unwrap();
    store.sent(a.id, Some("m1")).unwrap();
    assert_eq!(store.due_outbound(later, 10).unwrap()[0].id, b.id);
}

#[test]
fn an_attempt_in_flight_at_a_crash_becomes_unknown_and_is_not_resent() {
    let home = TestHome::new();
    let store = home.store("default");
    let (a, _) = store.enqueue_outbound(&outbound("s:t:0", "c")).unwrap();
    store.start_sending(a.id).unwrap();
    assert_eq!(store.abandon_in_flight().unwrap(), 1);
    let entry = store.outbound(a.id).unwrap().unwrap();
    assert_eq!(entry.state, OutboundState::Unknown);
    assert!(store.due_outbound(Timestamp::now(), 10).unwrap().is_empty());
    store.requeue_outbound(a.id).unwrap();
    assert_eq!(store.due_outbound(Timestamp::now(), 10).unwrap().len(), 1);
    store.drop_outbound(a.id).unwrap();
    assert_eq!(store.outbound(a.id).unwrap(), None);
    assert_eq!(
        store.requeue_outbound(a.id).unwrap_err().code(),
        &codes::GATEWAY_OUTBOX_ENTRY
    );
}
