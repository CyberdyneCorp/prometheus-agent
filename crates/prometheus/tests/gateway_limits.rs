//! The daemon-wide turn cap (spec: messaging-gateway, Concurrency caps
//! across surfaces), in the real daemon with the file-backed test
//! platform: turns beyond `gateway.limits.max_concurrent_sessions` wait
//! first in, first out and are told so.

#![cfg(feature = "gateway")]

mod common;

use std::time::Duration;

use serde_json::json;

use self::common::gateway::{Daemon, adapter_dir, gateway_home_with, sent, wait};

const RULES: &str = "[[rule]]\nuser_regex = '^slow'\nrepeat = true\nlatency_ms = 1500\ntext = \"slow done\"\n\n\
                     [[rule]]\nrepeat = true\ntext = \"fast done\"\n";

/// Drops a message of `sender` in its own DM.
fn dm(home: &std::path::Path, sender: &str, text: &str) {
    let inbox = adapter_dir(home).join("default").join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = json!({"id": format!("m-{sender}"), "sender": sender, "chat": sender,
        "chat_type": "dm", "text": text});
    let tmp = inbox.join(format!("{sender}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("{sender}.json"))).unwrap();
}

/// What the adapter sent to `chat`.
fn said_to(home: &std::path::Path, chat: &str) -> Vec<String> {
    sent(home, "default")
        .iter()
        .filter(|s| s["kind"] == "send" && s["chat"] == chat)
        .map(|s| s["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Spec scenario: Cap queues a turn.
#[test]
fn a_third_turn_waits_for_a_free_slot_and_then_runs() {
    let home = gateway_home_with(RULES, "");
    let home = home.path();
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.limits]\nmax_concurrent_sessions = 2\n",
    )
    .unwrap();
    let daemon = Daemon::start(home);
    dm(home, "u1", "slow one");
    dm(home, "u2", "slow two");
    wait("two turns run", Duration::from_secs(30), || {
        let status = common::run(home, &["--json", "daemon", "status"]);
        serde_json::from_slice::<serde_json::Value>(&status.stdout)
            .ok()
            .and_then(|s| s["daemon"]["active_turns"].as_u64())
            .is_some_and(|n| n >= 2)
    });
    dm(home, "u3", "slow three");
    wait(
        "the third chat is told to wait",
        Duration::from_secs(30),
        || {
            said_to(home, "u3")
                .iter()
                .any(|t| t == "Waiting for a free slot (1 ahead)")
        },
    );
    assert!(
        !said_to(home, "u3").iter().any(|t| t.contains("slow done")),
        "it did not run alongside the two"
    );
    wait("every chat is answered", Duration::from_mins(1), || {
        ["u1", "u2", "u3"]
            .iter()
            .all(|c| said_to(home, c).iter().any(|t| t.contains("slow done")))
    });
    daemon.stop(home);
}
