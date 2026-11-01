use std::sync::Arc;
use std::time::Duration;

use pmt_rpc::Catalog;
use pmt_rpc::catalog::{INDEX, IndexEntry, Kind};
use pmt_rpc::contracts::RequestCancel;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{Hub, Limits, Peer, Violations};

const S: &str = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W";

fn hub_with(limits: Limits) -> Arc<Hub> {
    let catalog = Arc::new(Catalog::builtin().clone());
    Arc::new(Hub::new(
        "epoch-a".into(),
        catalog,
        limits,
        Arc::new(Violations::new(false)),
    ))
}

fn hub() -> Arc<Hub> {
    hub_with(Limits::default())
}

fn peer(server_requests: bool) -> (Peer, mpsc::UnboundedReceiver<Message>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let peer = Peer {
        tx,
        minor: 0,
        advertised: Arc::default(),
        server_requests,
    };
    (peer, rx)
}

fn frames(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(Message::Text(text)) = rx.try_recv() {
        out.push(serde_json::from_str(text.as_str()).unwrap());
    }
    out
}

fn event(n: u64) -> Value {
    json!({"type": "test", "n": n})
}

#[test]
fn events_after_a_dropped_connection_are_replayed_in_order() {
    let hub = hub();
    hub.register(S, "default");
    let (p, mut rx) = peer(false);
    hub.attach(1, S, p);
    for n in 1..=3 {
        hub.publish(S, "session.event", event(n));
    }
    let seen: Vec<u64> = frames(&mut rx)
        .iter()
        .map(|f| f["params"]["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seen, [1, 2, 3]);
    hub.detach_all(1);
    for n in 4..=5 {
        hub.publish(S, "session.event", event(n));
    }
    let replay = hub.events_since(S, 3);
    let seqs: Vec<u64> = replay.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [4, 5]);
    assert_eq!(replay.events[1].data["n"], 5);
    assert_eq!(replay.latest_seq, 5);
    assert!(!replay.truncated);
    assert_eq!(replay.epoch, "epoch-a");
}

#[test]
fn evicted_events_mark_the_replay_truncated() {
    let hub = hub_with(Limits {
        session_events: 3,
        ..Limits::default()
    });
    for n in 1..=5 {
        hub.publish(S, "session.event", event(n));
    }
    let replay = hub.events_since(S, 0);
    let seqs: Vec<u64> = replay.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [3, 4, 5]);
    assert!(replay.truncated);
    assert!(!hub.events_since(S, 2).truncated);
}

#[test]
fn the_oldest_session_is_evicted_first_past_the_session_limit() {
    let hub = hub_with(Limits {
        sessions: 2,
        ..Limits::default()
    });
    for session in ["s1", "s2", "s3"] {
        hub.publish(session, "session.event", event(1));
    }
    assert!(hub.events_since("s1", 0).events.is_empty());
    assert!(hub.events_since("s1", 0).truncated);
    assert_eq!(hub.events_since("s3", 0).events.len(), 1);
}

#[tokio::test]
async fn without_a_capable_client_a_request_is_unanswered_at_once() {
    let hub = hub();
    let (p, mut rx) = peer(false);
    hub.attach(1, S, p);
    let asked = tokio::time::timeout(
        Duration::from_millis(100),
        hub.ask(
            S,
            "sudo.request",
            json!({"session_id": S, "command": "ls"}),
            Duration::from_mins(10),
        ),
    )
    .await
    .expect("resolved without waiting for the deadline");
    assert_eq!(asked, None);
    assert!(frames(&mut rx).is_empty(), "nothing was sent");
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_request_emits_one_cancel_and_drops_late_answers() {
    let hub = hub();
    let (p, mut rx) = peer(true);
    hub.attach(1, S, p);
    let params = json!({"session_id": S, "command": "ls"});
    let asked = hub
        .ask(S, "sudo.request", params, Duration::from_secs(5))
        .await;
    assert_eq!(asked, None);
    let sent = frames(&mut rx);
    let id = sent[0]["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("srq-") && id.len() == 16, "{id}");
    let cancels: Vec<&Value> = sent
        .iter()
        .filter(|f| f["method"] == "request.cancel")
        .collect();
    assert_eq!(cancels.len(), 1);
    let cancel: RequestCancel =
        serde_json::from_value(cancels[0]["params"]["data"].clone()).unwrap();
    assert_eq!(
        cancel,
        RequestCancel {
            id: id.clone(),
            reason: "timeout".into()
        }
    );
    assert!(!hub.resolve(1, &id, Ok(json!({"password": null}))));
}

#[tokio::test]
async fn the_first_answer_wins_and_the_others_see_it_resolved() {
    let hub = hub();
    let (a, mut rx_a) = peer(true);
    let (b, mut rx_b) = peer(true);
    hub.attach(1, S, a);
    hub.attach(2, S, b);
    let asking = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.ask(
                S,
                "sudo.request",
                json!({"session_id": S, "command": "ls"}),
                Duration::from_secs(5),
            )
            .await
        })
    };
    let request = loop {
        if let Some(Message::Text(text)) = rx_a.recv().await {
            break serde_json::from_str::<Value>(text.as_str()).unwrap();
        }
    };
    let id = request["id"].as_str().unwrap().to_owned();
    assert!(
        !hub.resolve(3, &id, Ok(json!({"password": "x"}))),
        "not sent to conn 3"
    );
    assert!(
        !hub.resolve(2, &id, Ok(json!({"pasword": "x"}))),
        "invalid answer dropped"
    );
    assert!(hub.resolve(2, &id, Ok(json!({"password": "pw"}))));
    assert_eq!(asking.await.unwrap(), Some(json!({"password": "pw"})));
    let b_frames = frames(&mut rx_b);
    assert!(b_frames.iter().any(|f| f["method"] == "request.resolved"));
}

#[test]
fn an_older_minor_gets_no_newer_events_unless_it_advertised_them() {
    let mut index = INDEX.to_vec();
    index.push(IndexEntry {
        name: "test.newer",
        kind: Kind::Event,
        owner: "workspace-architecture",
        since: 4,
    });
    let catalog = Catalog::build(
        pmt_rpc::contracts::Version { major: 1, minor: 4 },
        &index,
        |d| {
            d.event::<Newer>();
        },
    )
    .unwrap();
    let hub = Hub::new(
        "e".into(),
        Arc::new(catalog),
        Limits::default(),
        Arc::new(Violations::new(false)),
    );
    let (mut old, mut rx_old) = peer(false);
    old.minor = 2;
    let (mut aware, mut rx_aware) = peer(false);
    aware.minor = 2;
    aware.advertised = Arc::new(vec!["test.newer".into()]);
    hub.attach(1, S, old);
    hub.attach(2, S, aware);
    hub.publish(S, "test.newer", json!({}));
    assert!(frames(&mut rx_old).is_empty());
    assert_eq!(frames(&mut rx_aware).len(), 1);
}

struct Newer;

impl pmt_rpc::contracts::Event for Newer {
    const NAME: &'static str = "test.newer";
    type Payload = pmt_rpc::contracts::Empty;
}

#[test]
fn a_payload_that_breaks_its_contract_is_counted_not_sent() {
    let violations = Arc::new(Violations::new(false));
    let hub = Hub::new(
        "e".into(),
        Arc::new(Catalog::builtin().clone()),
        Limits::default(),
        Arc::clone(&violations),
    );
    let (p, mut rx) = peer(false);
    hub.attach(1, S, p);
    hub.publish(S, "request.cancel", json!({"id": 4}));
    assert_eq!(violations.count(), 1);
    assert!(frames(&mut rx).is_empty());
}

#[test]
fn one_connection_owns_a_session_until_it_detaches_or_disconnects() {
    let hub = hub();
    hub.register(S, "default");
    assert_eq!(hub.claim(1, S, Some(41)), Ok(()));
    assert_eq!(
        hub.claim(1, S, Some(41)),
        Ok(()),
        "the owner may claim again"
    );
    assert_eq!(hub.claim(2, S, Some(42)), Err(Some(41)));
    hub.detach(1, S);
    assert_eq!(hub.claim(2, S, Some(42)), Ok(()));
    hub.detach_all(2);
    assert_eq!(hub.claim(3, S, None), Ok(()));
}
