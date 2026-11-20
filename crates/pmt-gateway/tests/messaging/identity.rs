//! Cross-platform identity linking, delivery and read receipts, and busy
//! acknowledgement via reactions on the fake platform (spec:
//! messaging-gateway, Cross-platform identity linking; Delivery receipts
//! surfaced; platform-adapters, Delivery and read receipts; Busy
//! acknowledgement via reactions).

use std::time::Duration;

use pmt_gateway::messaging::{Admission, ChatType, Descriptor, Inbound, Receipt};
use pmt_store::gateway::outbox::OutboundState;

use crate::support::{FakeAdapter, Harness, Setup, eventually, said};

const SIGNAL: &str =
    "options = { http_url = \"http://127.0.0.1:8080\", account = \"+15550000001\" }\n";

/// Telegram and Signal, both open unless `signal_access` says otherwise,
/// with `extra` gateway keys.
fn two_platforms(signal_access: &str, extra: &str) -> String {
    format!(
        "[gateway]\n{extra}\n[gateway.busy]\ndebounce_ms = 0\n\n\
         [gateway.platforms.telegram]\nenabled = true\nallow_all = true\n\n\
         [gateway.platforms.signal]\nenabled = true\n{SIGNAL}{signal_access}\n"
    )
}

/// The 6-digit code the last `/link` reply names.
fn code_of(adapter: &FakeAdapter) -> String {
    let text = adapter
        .texts()
        .into_iter()
        .rev()
        .find(|t| t.starts_with("Your link code is "))
        .expect("a link code");
    text.trim_start_matches("Your link code is ")
        .chars()
        .take(6)
        .collect()
}

async fn link(h: &Harness) {
    h.dm("telegram", "tg-1", "/link").await;
    said(&h.adapter("telegram"), "Your link code is ").await;
    let code = code_of(&h.adapter("telegram"));
    h.dm("signal", "+15557770000", &format!("/link {code}"))
        .await;
    said(&h.adapter("signal"), "Linked: ").await;
}

/// Scenario: Continue on another platform.
#[tokio::test]
async fn a_linked_person_continues_one_conversation_on_another_platform() {
    let h = Harness::start(Setup::new(&two_platforms(
        "allow_all = true",
        "identity = { dm_mode = \"shared\" }",
    )))
    .await;
    link(&h).await;
    let store = h.store();
    let person = store.person_of("telegram", "tg-1").unwrap().unwrap();
    assert_eq!(
        store.person_of("signal", "+15557770000").unwrap(),
        Some(person)
    );

    h.dm("telegram", "tg-1", "list the open tickets").await;
    said(&h.adapter("telegram"), "echo: list the open tickets").await;
    h.dm("signal", "+15557770000", "and the second one?").await;
    said(&h.adapter("signal"), "echo: and the second one?").await;
    assert_eq!(
        h.sessions().len(),
        1,
        "one conversation across both platforms"
    );
    assert!(
        !h.adapter("telegram")
            .texts()
            .iter()
            .any(|t| t.contains("second one")),
        "the reply went to Signal only"
    );
    // Groups never merge, and an unlink splits the DMs again.
    h.dm("signal", "+15557770000", "/unlink telegram").await;
    said(&h.adapter("signal"), "Unlinked your telegram account.").await;
    assert_eq!(store.person_of("signal", "+15557770000").unwrap(), None);
}

/// Without `dm_mode = "shared"` linked accounts keep their own DMs.
#[tokio::test]
async fn per_platform_mode_keeps_each_accounts_dms_apart() {
    let h = Harness::start(Setup::new(&two_platforms("allow_all = true", ""))).await;
    link(&h).await;
    h.dm("telegram", "tg-1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    h.dm("signal", "+15557770000", "second").await;
    said(&h.adapter("signal"), "echo: second").await;
    assert_eq!(h.sessions().len(), 2);
}

/// Scenario: Linking cannot bypass authorization.
#[tokio::test]
async fn an_unauthorized_account_cannot_link() {
    let h = Harness::start(Setup::new(&two_platforms(
        "unauthorized_dm = \"decline\"",
        "identity = { dm_mode = \"shared\" }",
    )))
    .await;
    h.dm("telegram", "tg-1", "/link").await;
    said(&h.adapter("telegram"), "Your link code is ").await;
    let code = code_of(&h.adapter("telegram"));
    let admission = h
        .dm("signal", "+15557770000", &format!("/link {code}"))
        .await;
    assert_eq!(admission, Admission::Unauthorized);
    said(&h.adapter("signal"), "You are not authorized").await;
    assert!(h.store().identity_links().unwrap().is_empty());
}

/// Scenario: Expired code.
#[tokio::test]
async fn an_expired_code_links_nothing() {
    let h = Harness::start(Setup::new(&two_platforms("allow_all = true", ""))).await;
    h.dm("telegram", "tg-1", "/link").await;
    said(&h.adapter("telegram"), "Your link code is ").await;
    let code = code_of(&h.adapter("telegram"));
    // 601 seconds later.
    let later = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_secs(601))
        .unwrap();
    let store = h.store();
    assert_eq!(
        store
            .redeem_link_code(&code, ("signal", "+15557770000"), later)
            .unwrap(),
        pmt_store::gateway::identity::Redeemed::Expired
    );
    let fresh = store
        .link_code("telegram", "tg-1", jiff::Timestamp::now())
        .unwrap();
    let past = jiff::Timestamp::now()
        .checked_sub(jiff::SignedDuration::from_secs(700))
        .unwrap();
    // The chat path: a code issued 700 s ago.
    let stale = store.link_code("telegram", "tg-2", past).unwrap();
    h.dm("signal", "+15557770000", &format!("/link {stale}"))
        .await;
    said(&h.adapter("signal"), "Link code expired").await;
    assert!(store.identity_links().unwrap().is_empty());
    assert_ne!(fresh, stale);
}

fn with_receipts(platform: &str) -> Descriptor {
    let adapter = FakeAdapter::new(platform);
    let mut descriptor = adapter.descriptor.lock().unwrap().clone();
    descriptor.delivery_receipts = true;
    descriptor.read_receipts = true;
    descriptor
}

/// Scenario: Read receipt reaches the TUI; platform-adapters scenario
/// Signal read receipt (the outbox side).
#[tokio::test]
async fn a_read_receipt_moves_the_entry_to_read_and_reaches_attached_clients() {
    let mut setup = Setup::new(&two_platforms("allow_all = true", ""));
    setup.descriptors.push(with_receipts("signal"));
    let h = Harness::start(setup).await;
    h.dm("signal", "+15557770000", "hello").await;
    said(&h.adapter("signal"), "echo: hello").await;
    let session = h.sessions()[0].to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tui = pmt_gateway::hub::Peer {
        tx,
        features: std::sync::Arc::default(),
        minor: h.server.protocol().minor,
        advertised: std::sync::Arc::default(),
        server_requests: false,
        client_id: "tui-1".into(),
        role: pmt_rpc::contracts::AttachRole::Observer,
    };
    h.server.hub().attach(902_000, &session, tui, None);
    let store = h.store();
    let sent = || {
        store
            .outbox_entries(None, 50)
            .unwrap()
            .into_iter()
            .find(|e| e.body.contains("echo: hello") && e.platform_message_id.is_some())
    };
    eventually("the reply's send is recorded", || sent().is_some()).await;
    let entry = sent().unwrap();
    let id = entry.platform_message_id.clone().expect("a sent id");
    let link = h.adapter("signal").link.lock().unwrap().clone().unwrap();
    link.receipt(Receipt {
        message_id: "unknown-id".into(),
        read: true,
    });
    link.receipt(Receipt {
        message_id: id,
        read: true,
    });
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = rx
                .recv()
                .await
                .expect("a frame")
                .into_text()
                .unwrap()
                .to_string();
            if text.contains("\"message.delivery\"") {
                return text;
            }
        }
    })
    .await
    .expect("message.delivery");
    let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["params"]["data"]["state"], "read", "{frame}");
    assert_eq!(frame["params"]["data"]["outbox_id"], entry.id);
    assert_eq!(
        store.outbound(entry.id).unwrap().unwrap().state,
        OutboundState::Read
    );
}

/// Scenario: Reaction instead of noise.
#[tokio::test]
async fn a_follow_up_during_a_turn_gets_a_reaction_instead_of_a_busy_text() {
    let mut setup = Setup::new(&crate::support::config(
        "discord",
        "allow_all = true\n[gateway.platforms.discord.busy]\ninput_mode = \"queue\"",
    ));
    let adapter = FakeAdapter::new("discord");
    let mut descriptor = adapter.descriptor.lock().unwrap().clone();
    descriptor.supports_reactions = true;
    setup.descriptors.push(descriptor);
    let h = Harness::start(setup).await;
    h.dm("discord", "1", "slow:500").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    let follow_up = Inbound {
        message_id: "m-77".into(),
        ..Inbound::text(ChatType::Dm, "1", "1", "one more thing")
    };
    h.send("discord", follow_up).await;
    said(&h.adapter("discord"), "echo: one more thing").await;
    let reactions = h.adapter("discord").reactions.lock().unwrap().clone();
    assert!(
        reactions
            .iter()
            .any(|r| r.message_id == "m-77" && r.emoji == "\u{1f440}"),
        "{reactions:?}"
    );
    assert!(
        !h.adapter("discord")
            .texts()
            .iter()
            .any(|t| t.starts_with("Still working")),
        "no busy text"
    );
}

/// `gateway.ack_reaction` marks every accepted message.
#[tokio::test]
async fn ack_reaction_marks_each_accepted_message() {
    let mut setup = Setup::new(&crate::support::config(
        "discord",
        "allow_all = true\n[gateway]\nack_reaction = \"thumbsup\"",
    ));
    let adapter = FakeAdapter::new("discord");
    let mut descriptor = adapter.descriptor.lock().unwrap().clone();
    descriptor.supports_reactions = true;
    setup.descriptors.push(descriptor);
    let h = Harness::start(setup).await;
    let message = Inbound {
        message_id: "m-1".into(),
        ..Inbound::text(ChatType::Dm, "1", "1", "hi")
    };
    h.send("discord", message).await;
    said(&h.adapter("discord"), "echo: hi").await;
    let reactions = h.adapter("discord").reactions.lock().unwrap().clone();
    assert!(
        reactions
            .iter()
            .any(|r| r.message_id == "m-1" && r.emoji == "\u{1f44d}"),
        "{reactions:?}"
    );
}
