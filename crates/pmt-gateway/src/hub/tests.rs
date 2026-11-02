use std::sync::Arc;
use std::time::Duration;

use pmt_rpc::Catalog;
use pmt_rpc::catalog::{INDEX, IndexEntry, Kind};
use pmt_rpc::contracts::{AttachRole, RequestCancel};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{Answered, Hub, Limits, Peer, Since, Violations};

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
        client_id: "tui-1".into(),
        role: AttachRole::Driver,
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
    hub.attach(1, S, p, None);
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
    hub.attach(1, S, p, None);
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
    let sent = frames(&mut rx);
    assert_eq!(sent.len(), 1, "no request, only its settlement: {sent:?}");
    assert_eq!(sent[0]["method"], "request.cancel");
    assert_eq!(sent[0]["params"]["data"]["reason"], "no_responder");
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_request_emits_one_cancel_and_drops_late_answers() {
    let hub = hub();
    let (p, mut rx) = peer(true);
    hub.attach(1, S, p, None);
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
    assert_eq!(
        hub.resolve(1, "tui-1", &id, Ok(json!({"password": null}))),
        Answered::Settled
    );
    assert_eq!(
        hub.resolve(
            1,
            "tui-1",
            "srq-000000000000",
            Ok(json!({"password": null}))
        ),
        Answered::Unknown
    );
}

#[tokio::test]
async fn the_first_answer_wins_and_the_others_see_it_resolved() {
    let hub = hub();
    let (a, mut rx_a) = peer(true);
    let (b, mut rx_b) = peer(true);
    hub.attach(1, S, a, None);
    hub.attach(2, S, b, None);
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
    assert_eq!(
        hub.resolve(3, "tui-3", &id, Ok(json!({"password": "x"}))),
        Answered::Unknown,
        "not sent to conn 3"
    );
    assert_eq!(
        hub.resolve(2, "desktop-2", &id, Ok(json!({"pasword": "x"}))),
        Answered::Unknown,
        "invalid answer dropped"
    );
    assert_eq!(
        hub.resolve(2, "desktop-2", &id, Ok(json!({"password": "pw"}))),
        Answered::Taken
    );
    assert_eq!(asking.await.unwrap(), Some(json!({"password": "pw"})));
    let a_frames = frames(&mut rx_a);
    let resolved = a_frames
        .iter()
        .find(|f| f["method"] == "request.resolved")
        .expect("the other window hears who answered");
    assert_eq!(resolved["params"]["data"]["by_client"], "desktop-2");
    assert!(
        frames(&mut rx_b)
            .iter()
            .any(|f| f["method"] == "request.resolved")
    );
}

#[test]
fn an_older_minor_gets_no_newer_events_unless_it_advertised_them() {
    let mut index = INDEX.to_vec();
    index.push(IndexEntry {
        name: "test.newer",
        kind: Kind::Event,
        owner: "workspace-architecture",
        since: 4,
        long: false,
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
    hub.attach(1, S, old, None);
    hub.attach(2, S, aware, None);
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
    hub.attach(1, S, p, None);
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

#[test]
fn reattaching_replays_kept_events_after_the_watermark() {
    let hub = hub();
    hub.register(S, "default");
    for n in 1..=4 {
        hub.publish(S, "session.event", event(n));
    }
    let (p, mut rx) = peer(false);
    let since = Since {
        last_seen: 2,
        epoch: Some("epoch-a".into()),
    };
    let attached = hub.attach(1, S, p, Some(&since));
    assert!(!attached.truncated);
    assert_eq!(attached.latest_seq, 4);
    let seqs: Vec<u64> = frames(&mut rx)
        .iter()
        .map(|f| f["params"]["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [3, 4], "only what the client missed, in order");
}

#[test]
fn another_epoch_or_an_evicted_watermark_is_truncated_without_replay() {
    let hub = hub_with(Limits {
        session_events: 2,
        ..Limits::default()
    });
    hub.register(S, "default");
    for n in 1..=5 {
        hub.publish(S, "session.event", event(n));
    }
    let (p, mut rx) = peer(false);
    let restarted = Since {
        last_seen: 900,
        epoch: Some("epoch-before-restart".into()),
    };
    assert!(hub.attach(1, S, p, Some(&restarted)).truncated);
    assert!(frames(&mut rx).is_empty(), "stale deltas are not replayed");
    let (p, mut rx) = peer(false);
    let behind = Since {
        last_seen: 1,
        epoch: None,
    };
    assert!(hub.attach(2, S, p, Some(&behind)).truncated);
    assert!(frames(&mut rx).is_empty());
}

#[tokio::test]
async fn an_open_request_is_resent_to_a_driver_that_reattaches() {
    let hub = hub();
    hub.register(S, "default");
    let (a, mut rx_a) = peer(true);
    hub.attach(1, S, a, None);
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
    let sent = loop {
        if let Some(Message::Text(text)) = rx_a.recv().await {
            break serde_json::from_str::<Value>(text.as_str()).unwrap();
        }
    };
    assert_eq!(
        sent["params"]["deadline_ms"], 5000,
        "every request carries its deadline"
    );
    let id = sent["id"].as_str().unwrap().to_owned();
    let (observer, mut rx_o) = peer(true);
    let observer = Peer {
        role: AttachRole::Observer,
        ..observer
    };
    let attached = hub.attach(2, S, observer, None);
    assert_eq!(attached.open_requests.len(), 1, "listed to every attacher");
    assert!(frames(&mut rx_o).is_empty(), "never sent to an observer");
    let (b, mut rx_b) = peer(true);
    hub.attach(3, S, b, None);
    let resent = frames(&mut rx_b);
    assert_eq!(resent[0]["id"], id.as_str());
    assert_eq!(
        hub.resolve(3, "tui-3", &id, Ok(json!({"password": "pw"}))),
        Answered::Taken
    );
    assert_eq!(asking.await.unwrap(), Some(json!({"password": "pw"})));
}

#[tokio::test(start_paused = true)]
async fn a_session_without_clients_becomes_a_reaping_candidate() {
    let hub = hub();
    hub.register(S, "default");
    let (p, _rx) = peer(false);
    hub.attach(1, S, p, None);
    assert!(hub.unattended(Duration::ZERO).is_empty(), "attached");
    hub.detach_all(1);
    assert!(
        hub.unattended(Duration::from_mins(1)).is_empty(),
        "too soon"
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(
        hub.unattended(Duration::from_mins(1)),
        [(S.to_owned(), "default".to_owned())]
    );
    hub.closed(S);
    assert!(hub.unattended(Duration::ZERO).is_empty(), "closed once");
}

/// Only drivers answer: a connection asked as driver that re-attaches as
/// observer can no longer settle the request, and when nobody else can
/// answer it the request is withdrawn (`no_responder`) at once.
#[tokio::test]
async fn a_driver_that_reattaches_as_observer_can_no_longer_answer() {
    let hub = hub();
    hub.register(S, "default");
    let (driver, mut rx) = peer(true);
    hub.attach(1, S, driver.clone(), None);
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
    let sent = loop {
        if let Some(Message::Text(text)) = rx.recv().await {
            break serde_json::from_str::<Value>(text.as_str()).unwrap();
        }
    };
    let id = sent["id"].as_str().unwrap().to_owned();
    let observer = Peer {
        role: AttachRole::Observer,
        ..driver
    };
    let attached = hub.attach(1, S, observer, None);
    assert!(attached.open_requests.is_empty(), "withdrawn, not listed");
    let cancel = frames(&mut rx)
        .into_iter()
        .find(|f| f["method"] == "request.cancel")
        .expect("request.cancel");
    assert_eq!(
        cancel["params"]["data"]["reason"], "no_responder",
        "{cancel}"
    );
    assert_ne!(
        hub.resolve(1, "tui-1", &id, Ok(json!({"password": "pw"}))),
        Answered::Taken
    );
    assert_eq!(asking.await.unwrap(), None);
}

/// Detaching from one session leaves the requests of another session the
/// connection still drives open.
#[tokio::test]
async fn detaching_one_session_keeps_another_sessions_requests() {
    const OTHER: &str = "ses_other";
    let hub = hub();
    hub.register(S, "default");
    hub.register(OTHER, "default");
    let (driver, mut rx) = peer(true);
    hub.attach(1, S, driver.clone(), None);
    hub.attach(1, OTHER, driver, None);
    let asking = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.ask(
                OTHER,
                "sudo.request",
                json!({"session_id": OTHER, "command": "ls"}),
                Duration::from_secs(5),
            )
            .await
        })
    };
    let sent = loop {
        if let Some(Message::Text(text)) = rx.recv().await {
            break serde_json::from_str::<Value>(text.as_str()).unwrap();
        }
    };
    let id = sent["id"].as_str().unwrap().to_owned();
    hub.detach(1, S);
    assert_eq!(
        hub.resolve(1, "tui-1", &id, Ok(json!({"password": "pw"}))),
        Answered::Taken
    );
    assert_eq!(asking.await.unwrap(), Some(json!({"password": "pw"})));
}
