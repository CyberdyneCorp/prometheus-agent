//! Agent interop state (spec: agent-interop): inbound conversation
//! bindings, outbound exchanges, peer probes, the desktop relay's outbox
//! and inbox, and imported Hermes A2A sessions.

use jiff::{SignedDuration, Timestamp};
use pmt_store::a2a::{A2aBinding, A2aCall};
use pmt_store::bot_relay::{Envelope, RelayState, RelayTiming};
use pmt_store::{A2A_IMPORT_TAG, NewSession, Source};

use crate::support::{TestHome, session};

#[test]
fn bindings_are_exact_keys_never_titles() {
    let home = TestHome::new();
    let store = home.store("default");
    let alice = session(&store).id;
    let key = pmt_a2a_key("alice");
    assert_eq!(store.a2a_session(&key).unwrap(), None);
    store
        .bind_a2a(&A2aBinding {
            key: key.clone(),
            session_id: alice,
            peer: "alice".into(),
            tenant: None,
            context_id: "ctx-1".into(),
        })
        .unwrap();
    assert_eq!(store.a2a_session(&key).unwrap(), Some(alice));
    assert_eq!(store.a2a_session(&pmt_a2a_key("bob")).unwrap(), None);
    assert_eq!(store.a2a_bindings().unwrap().len(), 1);
}

fn pmt_a2a_key(peer: &str) -> String {
    format!("default:a2a:{peer}:ctx-1")
}

#[test]
fn outbound_exchanges_make_history_and_metrics() {
    let home = TestHome::new();
    let store = home.store("default");
    for (reply, error, ms) in [(Some("one"), None, 100), (None, Some("HTTP 500"), 300)] {
        store
            .record_a2a_call(&A2aCall {
                context_id: "c1".into(),
                agent: "researcher".into(),
                url: "https://r.example".into(),
                task_id: None,
                message: "hi".into(),
                reply: reply.map(str::to_owned),
                error: error.map(str::to_owned),
                duration_ms: ms,
                at: Timestamp::now(),
            })
            .unwrap();
    }
    let history = store.a2a_history("c1").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].reply.as_deref(), Some("one"));
    let metrics = store.a2a_metrics().unwrap();
    assert_eq!(
        (metrics[0].calls, metrics[0].failures, metrics[0].mean_ms),
        (2, 1, 200)
    );
    assert_eq!(store.a2a_conversations(10).unwrap()[0].exchanges, 2);
    store
        .record_peer_probe("spark", false, "unreachable")
        .unwrap();
    assert!(!store.peer_probe("spark").unwrap().unwrap().ok);
    store.forget_peer_probe("spark").unwrap();
    assert!(store.peer_probe("spark").unwrap().is_none());
}

fn envelope(id: &str, created: Timestamp) -> Envelope {
    Envelope {
        delivery_id: id.into(),
        sender: "@helper".into(),
        target: "@research@laptop".into(),
        message: "look".into(),
        signature: "sig".into(),
        created_at: created,
    }
}

const TIMING: RelayTiming = RelayTiming {
    deadline_ms: 1_560_000,
    ttl_ms: 900_000,
};

fn after(start: Timestamp, secs: i64) -> Timestamp {
    start + SignedDuration::from_secs(secs)
}

/// Scenario: Desktop closed mid-delivery.
#[test]
fn a_drained_envelope_is_offered_once_more_then_times_out() {
    let home = TestHome::new();
    let store = home.store("default");
    let t0 = Timestamp::now();
    store.relay_enqueue(&envelope("d1", t0)).unwrap();
    let first = store.relay_drain(after(t0, 10), TIMING).unwrap();
    assert_eq!(first.offered.len(), 1);
    // The courier quit: nothing is offered again before the deadline.
    assert!(
        store
            .relay_drain(after(t0, 1000), TIMING)
            .unwrap()
            .offered
            .is_empty()
    );
    let second = store.relay_drain(after(t0, 10 + 1560), TIMING).unwrap();
    assert_eq!(second.offered.len(), 1, "offered once more after 1560 s");
    assert!(
        store
            .relay_drain(after(t0, 10 + 3000), TIMING)
            .unwrap()
            .offered
            .is_empty()
    );
    assert!(
        store
            .relay_time_out(after(t0, 10 + 3100), TIMING)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.relay_time_out(after(t0, 10 + 3120), TIMING).unwrap(),
        ["d1"]
    );
    assert_eq!(
        store.relay_state("d1").unwrap(),
        Some(RelayState::Failed {
            reason: "delivery_timeout".into()
        })
    );
}

#[test]
fn queued_envelopes_expire_and_settles_need_the_signature() {
    let home = TestHome::new();
    let store = home.store("default");
    let t0 = Timestamp::now();
    store.relay_enqueue(&envelope("old", t0)).unwrap();
    store
        .relay_enqueue(&envelope("new", after(t0, 800)))
        .unwrap();
    let drained = store.relay_drain(after(t0, 901), TIMING).unwrap();
    assert_eq!(drained.expired, ["old"]);
    assert_eq!(drained.offered.len(), 1);
    assert_eq!(
        store.relay_state("old").unwrap(),
        Some(RelayState::Failed {
            reason: "queued_expired".into()
        })
    );
    assert!(!store.relay_settle("new", "forged", Ok("hi")).unwrap());
    assert!(store.relay_settle("new", "sig", Ok("hi")).unwrap());
    assert!(!store.relay_settle("new", "sig", Ok("again")).unwrap());
    assert_eq!(
        store.relay_state("new").unwrap(),
        Some(RelayState::Delivered { reply: "hi".into() })
    );
}

#[test]
fn a_delivery_id_is_received_once() {
    let home = TestHome::new();
    let store = home.store("default");
    assert!(store.relay_receive("d1", "s1").unwrap().is_none());
    let again = store.relay_receive("d1", "s2").unwrap().unwrap();
    assert_eq!(
        (again.session_id.as_str(), again.state.as_str()),
        ("s1", "running")
    );
    store.relay_received("d1", Ok("done")).unwrap();
    let done = store.relay_receive("d1", "s3").unwrap().unwrap();
    assert_eq!(done.reply.as_deref(), Some("done"));
}

#[test]
fn imported_hermes_a2a_sessions_are_counted_for_doctor() {
    let home = TestHome::new();
    let store = home.store("default");
    let mut new = NewSession::new(Source::Import);
    new.source_tag = Some(A2A_IMPORT_TAG.into());
    store.create_session(&new).unwrap();
    assert_eq!(store.unbound_a2a_imports().unwrap(), 1);
    let checks = store.doctor_checks().unwrap();
    assert!(checks.iter().any(|c| c.name == "a2a_imports"), "{checks:?}");
}
