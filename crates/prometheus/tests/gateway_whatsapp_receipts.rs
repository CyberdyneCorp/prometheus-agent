//! `WhatsApp` delivery and read receipts in the real daemon (spec:
//! messaging-gateway, Delivery receipts surfaced; platform-adapters,
//! Delivery and read receipts): the adapter talks to the mock bridge, which
//! queues bridge.js's `delivery_status` events; a client attached to the
//! `WhatsApp` session, as the TUI attaches, is told when the reply is read.

#![cfg(feature = "adapter-kits")]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_adapters_whatsapp::TOKEN_SECRET;
use pmt_adapters_whatsapp::mock::{self, BridgeMock};
use pmt_gateway::messaging::conformance::MockPlatform as _;
use serde_json::json;

use common::{append_config, daemon, mock_home, newest_session, ok, run, run_with_stdin};

const REPLY: &str = "the reply";

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_mins(1);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A mock home answering on `WhatsApp` through `bridge`.
fn whatsapp_home(bridge: &BridgeMock) -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(
        home.path().join("profiles/default/mock.toml"),
        format!("[[rule]]\nrepeat = true\ntext = \"{REPLY}\"\n"),
    )
    .unwrap();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", TOKEN_SECRET],
        &format!("{}\n", mock::TOKEN),
    ));
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = \"mock.toml\"\n\n[gateway.busy]\ndebounce_ms = 0\n\n\
             [gateway.platforms.whatsapp]\nenabled = true\nallow_all = true\n{}\n",
            bridge.table()
        ),
    );
    home
}

/// The outbox line of the reply, once it was sent.
fn reply_entry(home: &Path) -> Option<String> {
    ok(&run(home, &["gateway", "outbox", "list"]))
        .lines()
        .find(|line| line.contains("reply") && line.contains("whatsapp:"))
        .map(str::to_owned)
}

/// Scenario: Read receipt reaches the TUI.
#[test]
fn a_read_whatsapp_reply_reaches_the_attached_client_and_the_outbox() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let bridge: Arc<BridgeMock> = runtime.block_on(BridgeMock::start());
    let home = whatsapp_home(&bridge);
    let home = home.path();
    let _daemon = daemon::start(home, &[]);
    bridge.receive(&json!({"body": "hello"}));
    wait("the reply is sent", || {
        bridge.sent().iter().any(|m| m.text.contains(REPLY))
    });
    let sent_id = bridge
        .sent()
        .into_iter()
        .find(|m| m.text.contains(REPLY))
        .unwrap()
        .message_id;
    wait("the send is recorded", || {
        reply_entry(home).is_some_and(|line| line.contains("sent"))
    });
    let session = newest_session(home);
    let frame = runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home, "default").await;
        client
            .call("session.attach", json!({"session_id": session}))
            .await
            .unwrap();
        // An id the adapter never sent and a server ack are not receipts
        // of the reply; the read status is.
        bridge.status("WA-unknown", "read");
        bridge.status(&sent_id, "server_ack");
        bridge.status(&sent_id, "read");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = inbox.events.recv().await.expect("an event");
                if event.event == "message.delivery" {
                    return event.data;
                }
            }
        })
        .await
        .expect("message.delivery")
    });
    assert_eq!(frame["state"], "read", "{frame}");
    assert_eq!(frame["session_id"], session.as_str(), "{frame}");
    let line = reply_entry(home).unwrap();
    assert!(line.contains("read"), "{line}");
}
