//! Chats, frozen policies, sender spend and outbound attachments (spec:
//! messaging-gateway, Per-chat policies; Per-user rate limiting;
//! agent-interop, One-shot send command).

use jiff::{SignedDuration, Timestamp};
use pmt_core::id::SessionId;
use pmt_store::gateway::chats::KnownChat;
use pmt_store::gateway::outbox::{NewOutbound, OutboundAttachment, OutboundKind};
use pmt_store::gateway::routes::NewRoute;
use pmt_store::{CostStatus, NewSession, Origin, Source, Store, Tokens, UsageEvent};

use crate::support::TestHome;

fn gateway_session(store: &Store, platform: &str, chat: &str, user: &str) -> SessionId {
    let mut new = NewSession::new(Source::Gateway);
    new.origin = Origin::Gateway {
        platform: platform.into(),
        chat: chat.into(),
        thread: None,
        user: Some(user.into()),
    };
    let id = store.create_session(&new).unwrap().id;
    store
        .bind_route(
            &NewRoute {
                key: format!("k:{platform}:{chat}:{user}"),
                session_id: id,
                platform: platform.into(),
                chat_id: chat.into(),
                thread_id: None,
            },
            None,
        )
        .unwrap();
    id
}

fn spend(session: SessionId, call: &str, usd: f64, at: Timestamp) -> UsageEvent {
    UsageEvent {
        call_id: call.to_owned(),
        ts: at,
        session_id: session,
        turn_id: None,
        parent_call_id: None,
        purpose: "main".to_owned(),
        purpose_detail: None,
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        service_tier: None,
        tokens: Tokens::default(),
        cost_usd: Some(usd),
        cost_status: CostStatus::Exact,
        billing_source: "api_key:openai".to_owned(),
        request_hash: None,
        cache_hit: false,
    }
}

#[test]
fn a_senders_spend_counts_only_their_turns_since_the_cutoff() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::now();
    let mine = gateway_session(&store, "telegram", "1", "alice");
    let other = gateway_session(&store, "telegram", "2", "bob");
    let day_ago = now - SignedDuration::from_hours(48);
    store
        .record_gateway_turn(mine, "telegram", "alice", day_ago)
        .unwrap();
    store
        .record_gateway_turn(other, "telegram", "bob", day_ago)
        .unwrap();
    store.record_usage(&spend(mine, "a", 1.5, now)).unwrap();
    store.record_usage(&spend(mine, "b", 0.55, now)).unwrap();
    store
        .record_usage(&spend(mine, "old", 9.0, day_ago))
        .unwrap();
    store.record_usage(&spend(other, "c", 4.0, now)).unwrap();
    let since = now - SignedDuration::from_hours(1);
    let cost = store.sender_cost_since("telegram", "alice", since).unwrap();
    assert!((cost - 2.05).abs() < 1e-9, "{cost}");
    assert!(
        store
            .sender_cost_since("discord", "alice", since)
            .unwrap()
            .abs()
            < 1e-9
    );
}

/// A thread shared by several people: each turn's calls (and its
/// subagents') count against whoever sent that turn, not against the
/// person who started the thread's session.
#[test]
fn in_a_shared_thread_each_sender_pays_for_their_own_turns() {
    let home = TestHome::new();
    let store = home.store("default");
    let shared = gateway_session(&store, "slack", "C1", "alice");
    let t0 = Timestamp::now() - SignedDuration::from_mins(30);
    let at = |mins: i64| t0 + SignedDuration::from_mins(mins);
    store
        .record_gateway_turn(shared, "slack", "alice", at(0))
        .unwrap();
    store
        .record_usage(&spend(shared, "a1", 1.0, at(1)))
        .unwrap();
    store
        .record_gateway_turn(shared, "slack", "bob", at(5))
        .unwrap();
    store
        .record_usage(&spend(shared, "b1", 3.0, at(6)))
        .unwrap();
    // Bob's turn delegated to a subagent: its calls are Bob's too.
    let mut child = NewSession::new(Source::Gateway);
    child.parent = Some((shared, pmt_store::LineageKind::Delegate));
    let child = store.create_session(&child).unwrap().id;
    store.record_usage(&spend(child, "b2", 2.0, at(7))).unwrap();
    store
        .record_gateway_turn(shared, "slack", "alice", at(10))
        .unwrap();
    store
        .record_usage(&spend(shared, "a2", 0.5, at(11)))
        .unwrap();
    let since = t0 - SignedDuration::from_mins(1);
    let alice = store.sender_cost_since("slack", "alice", since).unwrap();
    let bob = store.sender_cost_since("slack", "bob", since).unwrap();
    assert!((alice - 1.5).abs() < 1e-9, "alice {alice}");
    assert!((bob - 5.0).abs() < 1e-9, "bob {bob}");
}

#[test]
fn a_frozen_policy_stays_with_its_session() {
    let home = TestHome::new();
    let store = home.store("default");
    let id = gateway_session(&store, "discord", "555", "u");
    assert_eq!(store.session_policy(id).unwrap(), None);
    store
        .set_session_policy(id, Some(r#"{"model":"a"}"#))
        .unwrap();
    assert_eq!(
        store.session_policy(id).unwrap().as_deref(),
        Some(r#"{"model":"a"}"#)
    );
}

#[test]
fn routed_chats_and_the_session_a_chat_routes_to() {
    let home = TestHome::new();
    let store = home.store("default");
    let first = gateway_session(&store, "telegram", "-100", "a");
    gateway_session(&store, "discord", "9", "b");
    assert_eq!(
        store.routed_chats(Some("telegram")).unwrap(),
        vec![KnownChat {
            platform: "telegram".into(),
            chat_id: "-100".into(),
            thread_id: None,
        }]
    );
    assert_eq!(store.routed_chats(None).unwrap().len(), 2);
    assert_eq!(
        store.chat_session("telegram", "-100", None).unwrap(),
        Some(first)
    );
    assert_eq!(store.chat_session("telegram", "-999", None).unwrap(), None);
}

#[test]
fn attachments_survive_the_outbox() {
    let home = TestHome::new();
    let store = home.store("default");
    let new = NewOutbound {
        idempotency_key: "send:1".into(),
        session_id: None,
        platform: "telegram".into(),
        chat_id: "1".into(),
        thread_id: None,
        reply_to: None,
        edit_of: None,
        kind: OutboundKind::Notification,
        body: "report".into(),
        attachments: vec![OutboundAttachment {
            path: "/data/report.png".into(),
            as_document: true,
            as_voice: false,
        }],
    };
    let (entry, _) = store.enqueue_outbound(&new).unwrap();
    assert_eq!(entry.attachments, new.attachments);
    let plain = NewOutbound {
        idempotency_key: "send:2".into(),
        attachments: Vec::new(),
        ..new
    };
    assert!(
        store
            .enqueue_outbound(&plain)
            .unwrap()
            .0
            .attachments
            .is_empty()
    );
}
