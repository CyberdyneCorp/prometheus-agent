//! Delivery (spec: messaging-gateway, Approval requests routed to every
//! surface; Streaming to platforms; Background and internal
//! notifications; Durable delivery outbox).

use std::sync::Arc;
use std::time::Duration;

use pmt_gateway::hub::Peer;
use pmt_gateway::messaging::{Admission, Background, BackgroundKind, ChatType, Inbound, SendError};
use pmt_rpc::contracts::AttachRole;
use pmt_store::gateway::outbox::OutboundState;
use serde_json::json;

use crate::support::{Did, Harness, Setup, config, eventually, open, said};

/// The id of the approval request open in the session, as the card's
/// buttons carry it.
fn card(h: &Harness) -> pmt_gateway::messaging::Outgoing {
    let did = h.adapter("telegram").all();
    did.iter()
        .find_map(|d| match d {
            Did::Send(m) if !m.buttons.is_empty() => Some(m.clone()),
            _ => None,
        })
        .expect("a card with buttons")
}

fn card_request(h: &Harness) -> String {
    card(h).buttons[0]
        .data
        .split(':')
        .nth(1)
        .unwrap()
        .to_owned()
}

/// The signed callback data of the card's `choice` button.
fn button(h: &Harness, choice: &str) -> String {
    card(h)
        .buttons
        .iter()
        .find(|b| b.data.contains(&format!(":{choice}:")))
        .expect("the choice's button")
        .data
        .clone()
}

#[tokio::test]
async fn an_approval_answered_in_the_desktop_app_updates_the_card() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let session = h.sessions()[0].to_string();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let desktop = Peer {
        tx,
        minor: h.server.protocol().minor,
        advertised: Arc::default(),
        server_requests: true,
        client_id: "desktop-900".into(),
        role: AttachRole::Driver,
    };
    h.server.hub().attach(900_000, &session, desktop, None);
    h.dm("telegram", "1", "approval").await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    let id = card_request(&h);
    let answered =
        h.server
            .hub()
            .resolve(900_000, "desktop-900", &id, Ok(json!({"decision": "once"})));
    assert_eq!(answered, pmt_gateway::hub::Answered::Taken);
    said(&h.adapter("telegram"), "approved").await;
    eventually("the card is edited", || {
        h.adapter("telegram").all().iter().any(|d| {
            matches!(d, Did::Edit(_, m) if m.text == "Approved once via desktop" && m.buttons.is_empty())
        })
    })
    .await;
}

#[tokio::test]
async fn an_unanswered_approval_expires_as_a_denial() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    *h.agents.approval_timeout.lock().unwrap() = Duration::from_millis(300);
    h.dm("telegram", "1", "approval").await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    said(&h.adapter("telegram"), "denied").await;
    eventually("the card is marked expired", || {
        h.adapter("telegram")
            .all()
            .iter()
            .any(|d| matches!(d, Did::Edit(_, m) if m.text.starts_with("Approval expired")))
    })
    .await;
}

#[tokio::test]
async fn a_bystander_cannot_approve() {
    let shared = "allow_all = true\n[gateway.platforms.telegram.sessions]\ngroup_per_user = false";
    let h = Harness::start(Setup::new(&config("telegram", shared))).await;
    h.send(
        "telegram",
        Inbound::text(ChatType::Group, "g1", "A", "approval"),
    )
    .await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    h.send(
        "telegram",
        Inbound::text(ChatType::Group, "g1", "B", "/approve"),
    )
    .await;
    said(
        &h.adapter("telegram"),
        "Only the person who asked or an admin may decide this.",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !h.adapter("telegram")
            .texts()
            .contains(&"approved".to_owned())
    );
    h.send(
        "telegram",
        Inbound::text(ChatType::Group, "g1", "A", "/approve"),
    )
    .await;
    said(&h.adapter("telegram"), "approved").await;
}

#[tokio::test]
async fn a_button_press_decides_too() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "approval").await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
    press.callback = Some(button(&h, "deny"));
    h.send("telegram", press).await;
    said(&h.adapter("telegram"), "denied").await;
}

#[tokio::test]
async fn a_forged_callback_leaves_the_approval_pending() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "approval").await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    let id = card_request(&h);
    let genuine = button(&h, "always");
    assert!(genuine.starts_with(&format!("appr:{id}:always:")));
    for forged in [
        format!("appr:{id}:always:deadbeef"),
        format!("appr:{id}:always"),
        genuine.replace(":always:", ":once:"),
    ] {
        let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
        press.callback = Some(forged);
        h.send("telegram", press).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let texts = h.adapter("telegram").texts();
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("approved") || t.contains("Approved")),
        "{texts:?}"
    );
    h.dm("telegram", "1", "/approve").await;
    said(&h.adapter("telegram"), "approved").await;
}

#[tokio::test]
async fn a_platform_cannot_turn_streaming_on_by_itself() {
    let table = "allow_all = true\n[gateway.platforms.telegram.streaming]\nenabled = true";
    let h = Harness::start(Setup::new(&config("telegram", table))).await;
    h.dm("telegram", "1", "stream").await;
    said(
        &h.adapter("telegram"),
        "hello world, this is a streamed answer",
    )
    .await;
    let did = h.adapter("telegram").all();
    assert_eq!(did.len(), 1, "only the final message: {did:?}");
}

#[tokio::test]
async fn the_final_answer_edits_the_streamed_draft() {
    let config =
        open("telegram") + "\n[gateway.streaming]\nenabled = true\nedit_interval_ms = 100\n";
    let h = Harness::start(Setup::new(&config)).await;
    h.dm("telegram", "1", "stream").await;
    eventually("the final edit", || {
        h.adapter("telegram").all().iter().any(
            |d| matches!(d, Did::Edit(_, m) if m.text == "hello world, this is a streamed answer"),
        )
    })
    .await;
    let sends: Vec<String> = h.adapter("telegram").texts();
    assert_eq!(sends.len(), 1, "one draft, no duplicate final: {sends:?}");
    assert!(
        sends[0].ends_with(" \u{2589}"),
        "the draft shows the cursor"
    );
}

#[tokio::test]
async fn errors_only_skips_a_successful_background_exit() {
    let config = open("telegram") + "\n[gateway.notifications]\nbackground = \"error\"\n";
    let h = Harness::start(Setup::new(&config)).await;
    h.dm("telegram", "1", "hi").await;
    said(&h.adapter("telegram"), "echo: hi").await;
    let session = h.sessions()[0].to_string();
    let done = Background {
        kind: BackgroundKind::Result,
        text: "build finished (exit 0)".into(),
    };
    assert_eq!(
        h.gateway.notify("default", &session, done),
        Admission::Accepted
    );
    let failed = Background {
        kind: BackgroundKind::Error,
        text: "tests failed (exit 1)".into(),
    };
    assert_eq!(
        h.gateway.notify("default", &session, failed),
        Admission::Accepted
    );
    said(&h.adapter("telegram"), "tests failed (exit 1)").await;
    assert!(
        !h.adapter("telegram")
            .texts()
            .contains(&"build finished (exit 0)".to_owned())
    );
}

#[tokio::test]
async fn a_notification_never_displaces_a_waiting_follow_up() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "slow:3000").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    let session = h.sessions()[0].to_string();
    h.dm("telegram", "1", "and then?").await;
    let event = Background {
        kind: BackgroundKind::Error,
        text: "failed".into(),
    };
    eventually("the follow-up waits", || {
        h.gateway.notify("default", &session, event.clone()) == Admission::Dropped
    })
    .await;
    h.dm("telegram", "1", "/stop").await;
    said(&h.adapter("telegram"), "Stopped.").await;
    let accepted = h.gateway.notify("default", &session, event);
    assert_ne!(accepted, Admission::Dropped, "taken once nothing waits");
}

#[tokio::test]
async fn an_ambiguous_send_is_not_repeated() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.adapter("telegram")
        .failures
        .lock()
        .unwrap()
        .push_back(SendError::Ambiguous("timed out after sending".into()));
    h.dm("telegram", "1", "hi").await;
    eventually("the entry is unknown", || {
        h.store()
            .outbox_entries(Some(OutboundState::Unknown), 10)
            .unwrap()
            .len()
            == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(h.adapter("telegram").texts().is_empty(), "never resent");
}

#[tokio::test]
async fn replies_wait_out_an_outage_and_arrive_in_order() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let adapter = h.adapter("telegram");
    adapter.failures.lock().unwrap().extend([
        SendError::Server("503".into()),
        SendError::Server("503".into()),
    ]);
    h.dm("telegram", "1", "/queue one").await;
    h.dm("telegram", "1", "two").await;
    let replies = || -> Vec<String> {
        h.adapter("telegram")
            .texts()
            .into_iter()
            .filter(|t| t.starts_with("echo:"))
            .collect()
    };
    eventually("both replies arrive", || replies().len() == 2).await;
    assert_eq!(replies(), vec!["echo: one", "echo: two"]);
    let sent = h
        .store()
        .outbox_entries(Some(OutboundState::Sent), 10)
        .unwrap();
    assert!(
        sent.iter().all(|e| e.attempts == 0),
        "an outage uses no attempts"
    );
}
