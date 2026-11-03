//! What the model sees of a messaging conversation, in the real daemon
//! with the mock provider (spec: messaging-gateway, Runtime metadata
//! footer; Message timestamps in model context).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::Value;

use self::common::gateway::{Daemon, gateway_home, send, texts, wait_said};
use self::common::{append_config, session_messages};

/// The stored messages of every session, oldest session first.
fn all_messages(home: &Path) -> Vec<Vec<Value>> {
    let listed = common::ok(&common::run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let mut sessions: Vec<(String, String)> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let started = s["started_at"].as_str().unwrap_or_default().to_owned();
            (started, s["id"].as_str().unwrap().to_owned())
        })
        .collect();
    sessions.sort();
    sessions
        .iter()
        .map(|(_, id)| session_messages(home, id))
        .collect()
}

/// Waits until the agent answered `n` messages.
fn replies(home: &Path, n: usize) {
    common::gateway::wait("the replies", std::time::Duration::from_mins(1), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.starts_with("fast done"))
            .count()
            >= n
    });
}

fn of_role<'a>(messages: &'a [Value], role: &str) -> Vec<&'a Value> {
    messages.iter().filter(|m| m["role"] == role).collect()
}

#[test]
fn the_footer_is_sent_but_never_stored_in_the_transcript() {
    let home = gateway_home("");
    let home = home.path();
    append_config(home, "\n[gateway.display]\nruntime_footer = true\n");
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "first");
    wait_said(home, "default", "fast done");
    send(home, "default", "u1", "second");
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    while texts(home, "default").len() < 2 {
        assert!(std::time::Instant::now() < deadline, "no second reply");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let replies = texts(home, "default");
    assert!(
        replies.iter().all(|t| t.starts_with("fast done\n\n")),
        "a footer follows each reply: {replies:?}"
    );
    let footer = replies[0].trim_start_matches("fast done\n\n").to_owned();
    assert!(!footer.is_empty());
    daemon.stop(home);
    let sessions = all_messages(home);
    let assistant = of_role(&sessions[0], "assistant");
    assert_eq!(assistant.len(), 2);
    for message in assistant {
        assert_eq!(
            message["content"], "fast done",
            "the transcript has no footer"
        );
    }
}

#[test]
fn timestamps_apply_to_sessions_created_after_they_are_enabled() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "one");
    wait_said(home, "default", "fast done");
    append_config(home, "\n[gateway.message_timestamps]\nenabled = true\n");
    common::gateway::wait(
        "the daemon reloads its config",
        std::time::Duration::from_secs(30),
        || {
            let status = common::run(home, &["--json", "daemon", "status"]);
            serde_json::from_slice::<Value>(&status.stdout)
                .ok()
                .and_then(|s| s["daemon"]["config"][0]["reloads"].as_u64())
                .is_some_and(|n| n > 0)
        },
    );
    send(home, "default", "u1", "two");
    replies(home, 2);
    send(home, "default", "u1", "/new");
    wait_said(home, "default", "New session started.");
    send(home, "default", "u1", "three");
    replies(home, 3);
    daemon.stop(home);
    let sessions = all_messages(home);
    assert_eq!(sessions.len(), 2);
    let old: Vec<String> = of_role(&sessions[0], "user")
        .iter()
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(old, ["one", "two"], "the existing session stays unchanged");
    let new = of_role(&sessions[1], "user");
    let content = new[0]["content"].as_str().unwrap();
    assert!(
        content.starts_with('[') && content.ends_with("] three"),
        "{content}"
    );
    assert_eq!(
        new[0]["metadata"]["user_text"], "three",
        "the text stays clean"
    );
    assert!(new[0]["metadata"]["author"]["sent_at"].is_string());
}
