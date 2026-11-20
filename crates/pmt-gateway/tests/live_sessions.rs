//! Shared live sessions and child transcripts at the protocol level
//! (spec: tui, Shared live sessions; Split panes for subagents and tool
//! output): every attached client hears `session.presence` as clients
//! join and leave, the running turn's events reach each of them with the
//! same `seq`, observers cannot drive, and `subagent.subscribe` needs an
//! attachment.

mod support;

use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::{ClientKind, Envelope};
use serde_json::{Value, json};

use self::support::Daemon;

async fn next_event(inbox: &mut Inbox, name: &str) -> Envelope {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), inbox.events.recv())
            .await
            .expect("an event within 5 s")
            .expect("the connection is open");
        if event.event == name {
            return event;
        }
    }
}

/// The `(client_kind, role)` pairs of a presence event.
fn listed(event: &Envelope) -> Vec<(String, String)> {
    event.data["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["client_kind"].as_str().unwrap().to_owned(),
                c["role"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn create(client: &Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

/// The `seq` of every `session.event` until the turn finishes.
async fn turn_seqs(inbox: &mut Inbox) -> Vec<u64> {
    let mut seqs = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), inbox.events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.event.as_str() {
            "session.event" => seqs.push(event.seq),
            "turn.finished" => return seqs,
            _ => {}
        }
    }
}

/// Scenario: Desktop joins a TUI session.
#[tokio::test]
async fn a_desktop_joining_a_tui_session_is_listed_to_both_and_gets_the_same_turn_seqs() {
    let daemon = Daemon::start().await;
    let (tui, mut tui_inbox) = daemon.client_as(ClientKind::Tui, "default", true).await;
    let session = create(&tui).await;
    let alone = next_event(&mut tui_inbox, "session.presence").await;
    assert_eq!(listed(&alone), [("tui".to_owned(), "driver".to_owned())]);
    let (desktop, mut desktop_inbox) = daemon.client_as(ClientKind::Desktop, "default", true).await;
    let attached = desktop
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    let both = [
        ("tui".to_owned(), "driver".to_owned()),
        ("desktop".to_owned(), "driver".to_owned()),
    ];
    for inbox in [&mut tui_inbox, &mut desktop_inbox] {
        let presence = next_event(inbox, "session.presence").await;
        assert_eq!(presence.data["session_id"], Value::from(session.clone()));
        assert_eq!(listed(&presence), both);
        // Daemon-level: no session seq is spent on it.
        assert_eq!(presence.session_id, None);
    }
    let desktop_id = attached["client_id"].as_str().unwrap().to_owned();
    assert!(desktop_id.starts_with("desktop-"), "{desktop_id}");
    tui.call(
        "prompt.submit",
        json!({"session_id": session, "text": "steps:3:20"}),
    )
    .await
    .unwrap();
    let from_tui = turn_seqs(&mut tui_inbox).await;
    let from_desktop = turn_seqs(&mut desktop_inbox).await;
    assert_eq!(from_tui.len(), 3);
    assert_eq!(from_tui, from_desktop);
    desktop
        .call("session.detach", json!({"session_id": session}))
        .await
        .unwrap();
    let left = next_event(&mut tui_inbox, "session.presence").await;
    assert_eq!(listed(&left), [("tui".to_owned(), "driver".to_owned())]);
}

/// Scenario: Observer cannot submit.
#[tokio::test]
async fn an_observer_is_listed_as_one_and_cannot_submit() {
    let daemon = Daemon::start().await;
    let (owner, mut owner_inbox) = daemon.client_as(ClientKind::Tui, "default", true).await;
    let session = create(&owner).await;
    let (watcher, _watcher_inbox) = daemon.client_as(ClientKind::Tui, "default", true).await;
    watcher
        .call(
            "session.attach",
            json!({"session_id": session, "role": "observer"}),
        )
        .await
        .unwrap();
    next_event(&mut owner_inbox, "session.presence").await;
    let presence = next_event(&mut owner_inbox, "session.presence").await;
    assert_eq!(
        listed(&presence)[1],
        ("tui".to_owned(), "observer".to_owned())
    );
    let err = watcher
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "hello"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    assert_eq!(err.message, "observers cannot submit input");
}

/// Scenario: Foreign subagent subscription.
#[tokio::test]
async fn subscribing_to_a_child_of_an_unattached_session_is_session_not_attached() {
    let daemon = Daemon::start().await;
    let (owner, _owner_inbox) = daemon.client("default", false).await;
    let session = create(&owner).await;
    let (stranger, _inbox) = daemon.client("default", false).await;
    let err = stranger
        .call(
            "subagent.subscribe",
            json!({"session_id": session, "subagent_id": "sa-01JB7Z3Q5X8V2K4M6N8P0R2T00"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    assert_eq!(
        err.data.as_ref().map(|d| d.code.as_str()),
        Some("session.not_attached")
    );
}
