//! Daemon-owned terminals over the client protocol (spec: desktop-app,
//! Embedded terminals are daemon-owned PTYs): drivers open and type into
//! a shell, the output reaches attached clients and is replayed to a
//! later one, observers cannot type, and a daemon keeps at most 16.

#![cfg(unix)]

mod support;

use std::time::Duration;

use base64::Engine as _;
use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

use self::support::Daemon;

async fn create(client: &Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

/// Waits for `needle` in the terminal's output events.
async fn output_until(inbox: &mut Inbox, needle: &str) -> String {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let event = tokio::time::timeout_at(deadline, inbox.events.recv())
            .await
            .expect("terminal output in time")
            .expect("open connection");
        if event.event != "pty.output" {
            continue;
        }
        let data = event.data["data"].as_str().unwrap_or_default();
        seen.extend(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
        );
        let text = String::from_utf8_lossy(&seen).into_owned();
        if text.contains(needle) {
            return text;
        }
    }
}

/// Scenario: Seventeenth terminal. Scenario: Observer cannot type.
#[tokio::test]
async fn terminals_are_driver_written_replayed_and_capped_at_sixteen() {
    let daemon = Daemon::start().await;
    let (driver, mut inbox) = daemon.client("default", false).await;
    let session = create(&driver).await;
    driver
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    let opened = driver
        .call("pty.open", json!({"session_id": session}))
        .await
        .unwrap();
    let pty = opened["pty_id"].as_str().unwrap().to_owned();
    let input = json!({"session_id": session, "pty_id": pty, "data": "echo marker-$((40+2))\n"});
    driver.call("pty.input", input).await.unwrap();
    output_until(&mut inbox, "marker-42").await;

    // A later client gets the recent output first.
    let (observer, _observer_inbox) = daemon.client("default", false).await;
    observer
        .call(
            "session.attach",
            json!({"session_id": session, "role": "observer"}),
        )
        .await
        .unwrap();
    let attached = observer
        .call("pty.attach", json!({"session_id": session, "pty_id": pty}))
        .await
        .unwrap();
    let replay = base64::engine::general_purpose::STANDARD
        .decode(attached["replay"].as_str().unwrap())
        .unwrap();
    assert!(String::from_utf8_lossy(&replay).contains("marker-42"));
    let refused = observer
        .call(
            "pty.input",
            json!({"session_id": session, "pty_id": pty, "data": "x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, -32_002);
    assert_eq!(refused.message, "observers cannot write to terminals");
    let refused = observer
        .call("pty.open", json!({"session_id": session}))
        .await
        .unwrap_err();
    assert_eq!(refused.message, "observers cannot write to terminals");

    let mut ids = vec![pty];
    for _ in 1..16 {
        let opened: Value = driver
            .call("pty.open", json!({"session_id": session}))
            .await
            .unwrap();
        ids.push(opened["pty_id"].as_str().unwrap().to_owned());
    }
    let refused = driver
        .call("pty.open", json!({"session_id": session}))
        .await
        .unwrap_err();
    assert_eq!(
        refused.message,
        "terminal limit reached (16); close a terminal first"
    );
    let listed = driver
        .call("pty.list", json!({"session_id": session}))
        .await
        .unwrap();
    assert_eq!(listed["terminals"].as_array().unwrap().len(), 16);
    for id in &ids {
        driver
            .call("pty.close", json!({"session_id": session, "pty_id": id}))
            .await
            .unwrap();
    }
    driver
        .call("pty.open", json!({"session_id": session}))
        .await
        .unwrap();
}

/// Waits for a `pty.detached` event naming `pty`.
async fn detached(inbox: &mut Inbox, pty: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let event = tokio::time::timeout_at(deadline, inbox.events.recv())
            .await
            .expect("pty.detached in time")
            .expect("open connection");
        if event.event == "pty.detached" && event.data["pty_id"] == pty {
            return;
        }
    }
}

/// Scenario support (web-dashboard, Webapp file staging and terminals):
/// one attachment per terminal; a second tab attaching takes it over, the
/// first is told and may type again only after attaching again.
#[tokio::test]
async fn a_second_driver_attaching_takes_the_terminal_over() {
    let daemon = Daemon::start().await;
    let (first, mut first_inbox) = daemon.client("default", false).await;
    let session = create(&first).await;
    let attach = json!({"session_id": session});
    first.call("session.attach", attach.clone()).await.unwrap();
    let opened = first
        .call("pty.open", json!({"session_id": session}))
        .await
        .unwrap();
    let pty = opened["pty_id"].as_str().unwrap().to_owned();
    let input = |data: &str| json!({"session_id": session, "pty_id": pty, "data": data});
    first
        .call("pty.input", input("echo one-$((0+1))\n"))
        .await
        .unwrap();
    output_until(&mut first_inbox, "one-1").await;

    let (second, mut second_inbox) = daemon.client("default", false).await;
    second.call("session.attach", attach).await.unwrap();
    let pty_ref = json!({"session_id": session, "pty_id": pty});
    second.call("pty.attach", pty_ref.clone()).await.unwrap();
    detached(&mut first_inbox, &pty).await;
    let refused = first.call("pty.input", input("x")).await.unwrap_err();
    assert!(
        refused.message.contains("took this terminal over"),
        "{refused:?}"
    );
    second
        .call("pty.input", input("echo two-$((1+1))\n"))
        .await
        .unwrap();
    output_until(&mut second_inbox, "two-2").await;

    // Attaching again takes it back.
    first.call("pty.attach", pty_ref).await.unwrap();
    detached(&mut second_inbox, &pty).await;
    first
        .call("pty.input", input("echo three-$((1+2))\n"))
        .await
        .unwrap();
    output_until(&mut first_inbox, "three-3").await;
    let refused = second.call("pty.input", input("x")).await.unwrap_err();
    assert!(
        refused.message.contains("took this terminal over"),
        "{refused:?}"
    );
}
