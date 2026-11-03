//! Who may talk (spec: messaging-gateway, Authorization order and
//! fail-closed default; Documented pairing flow).

use std::io::Write;
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use pmt_gateway::messaging::{Admission, ChatType, Inbound};

use crate::support::{Harness, Setup, config, eventually, said};

#[tokio::test]
async fn nobody_is_admitted_by_default() {
    let h = Harness::start(Setup::new(&config("discord", ""))).await;
    let mention = Inbound::text(ChatType::Channel, "555", "stranger", "@bot hi");
    assert_eq!(h.send("discord", mention).await, Admission::Unauthorized);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(h.agents.texts().is_empty(), "no turn");
    assert!(
        h.adapter("discord").texts().is_empty(),
        "silence in a group"
    );
}

#[tokio::test]
async fn an_unpaired_sender_is_refused_and_a_paired_one_admitted() {
    let h = Harness::start(Setup::new(&config("telegram", ""))).await;
    assert_eq!(h.dm("telegram", "42", "hi").await, Admission::PairingIssued);
    said(&h.adapter("telegram"), "Your pairing code is ").await;
    let text = h.adapter("telegram").texts()[0].clone();
    let code: String = text
        .split("Your pairing code is ")
        .nth(1)
        .unwrap()
        .chars()
        .take(8)
        .collect();
    assert!(h.agents.texts().is_empty(), "no turn before pairing");
    h.store()
        .approve_pairing("telegram", &code.to_lowercase(), Timestamp::now())
        .unwrap();
    assert_eq!(h.dm("telegram", "42", "hello").await, Admission::Accepted);
    said(&h.adapter("telegram"), "echo: hello").await;
    assert_eq!(
        h.dm("telegram", "43", "me too").await,
        Admission::PairingIssued,
        "another sender still needs a code"
    );
}

#[tokio::test]
async fn decline_and_ignore_policies_issue_no_code() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "unauthorized_dm = \"decline\"",
    )))
    .await;
    assert_eq!(h.dm("telegram", "42", "hi").await, Admission::Unauthorized);
    said(&h.adapter("telegram"), "You are not authorized").await;
    assert!(h.store().pairings(None).unwrap().is_empty());
    let h = Harness::start(Setup::new(&config("email", ""))).await;
    assert_eq!(h.dm("email", "a@b.c", "hi").await, Admission::Unauthorized);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        h.adapter("email").texts().is_empty(),
        "email ignores by default"
    );
}

#[tokio::test]
async fn a_sixth_bot_message_in_a_minute_is_refused() {
    // Bots admitted at all, so the loop guard is what refuses the sixth.
    let config = format!(
        "[gateway.bots]\nallow_bots = \"all\"\nrequire_inline_mention = false\n\n{}",
        config("discord", "allow_all = true")
    );
    let h = Harness::start(Setup::new(&config)).await;
    for n in 0..5 {
        let mut message = Inbound::text(ChatType::Channel, "555", "otherbot", "ping");
        message.is_bot = true;
        let admission = h.send("discord", message).await;
        assert_ne!(admission, Admission::Dropped, "message {n}");
    }
    let mut sixth = Inbound::text(ChatType::Channel, "555", "otherbot", "ping");
    sixth.is_bot = true;
    assert_eq!(h.send("discord", sixth).await, Admission::Dropped);
    let human = Inbound::text(ChatType::Channel, "555", "person", "hi");
    assert_ne!(h.send("discord", human).await, Admission::Dropped);
}

#[tokio::test]
async fn pair_approve_in_chat_is_for_admins() {
    let table = "allow_users = [\"111\", \"222\"]\nadmins = [\"111\"]\nunauthorized_dm = \"pair\"";
    let h = Harness::start(Setup::new(&config("telegram", table))).await;
    assert_eq!(h.dm("telegram", "42", "hi").await, Admission::PairingIssued);
    said(&h.adapter("telegram"), "Your pairing code is ").await;
    let text = h.adapter("telegram").texts()[0].clone();
    let code: String = text
        .split("Your pairing code is ")
        .nth(1)
        .unwrap()
        .chars()
        .take(8)
        .collect();
    h.dm("telegram", "222", &format!("/pair approve {code}"))
        .await;
    said(
        &h.adapter("telegram"),
        "This command is restricted to admins.",
    )
    .await;
    h.dm("telegram", "111", &format!("/pair approve {code}"))
        .await;
    said(&h.adapter("telegram"), "Approved 42 on telegram.").await;
    said(&h.adapter("telegram"), "You are paired").await;
    assert!(h.store().is_paired("telegram", "42").unwrap());
}

/// Captures every log line of the test's thread.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_pairing_code_is_never_logged() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let h = Harness::start(Setup::new(&config("telegram", ""))).await;
    h.dm("telegram", "42", "hi").await;
    said(&h.adapter("telegram"), "Your pairing code is ").await;
    let text = h.adapter("telegram").texts()[0].clone();
    let code: String = text
        .split("Your pairing code is ")
        .nth(1)
        .unwrap()
        .chars()
        .take(8)
        .collect();
    eventually("something was logged", || {
        !captured.0.lock().unwrap().is_empty()
    })
    .await;
    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    assert!(logs.contains("pairing code issued"), "{logs}");
    assert!(!logs.contains(&code), "the code is in the logs");
    let outbox = h.store().outbox_entries(None, 100).unwrap();
    assert!(
        outbox.iter().all(|e| !e.body.contains(&code)),
        "the code is never stored in the outbox"
    );
}
