//! Optional PII redaction in a real daemon with the mock provider (spec:
//! security-boundaries, Optional PII redaction).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::json;

use self::common::gateway::{Daemon, adapter_dir, gateway_home_with, sent, wait};

/// The model answers `leaked` when it sees the raw id or phone number,
/// `redacted` when it sees both pseudonymized.
const RULES: &str = r#"[[rule]]
user_regex = '123456789|415 555 0100|555-123-4567'
repeat = true
text = "leaked"

[[rule]]
user_regex = '\[user_[0-9a-f]{10}\|user_[0-9a-f]{10}\] call me at \[REDACTED:phone\]'
repeat = true
text = "redacted"

[[rule]]
repeat = true
text = "unmatched"
"#;

fn post(home: &Path, n: usize, text: &str, mentioned: bool) {
    let inbox = adapter_dir(home).join("default").join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = json!({"id": format!("m{n}"), "sender": "123456789", "chat": "grp",
        "chat_type": "group", "text": text, "mentioned": mentioned});
    let tmp = inbox.join(format!("m{n:04}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("m{n:04}.json"))).unwrap();
}

/// Scenario: Group chat.
#[test]
fn a_group_message_reaches_the_provider_pseudonymized_and_the_reply_reaches_the_chat() {
    let home = gateway_home_with(
        RULES,
        "\n[privacy]\nredact_pii = true\n\n[gateway.groups.platforms.test]\nobserve_unmentioned = true\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    post(home, 1, "call me at +1 415 555 0100", false);
    // The observed line is kept before the next mention is read.
    std::thread::sleep(Duration::from_millis(500));
    post(home, 2, "what is my number? not 555-123-4567", true);
    wait("the reply", Duration::from_mins(1), || {
        sent(home, "default").iter().any(|v| v["kind"] == "send")
    });
    daemon.stop(home);
    let replies: Vec<_> = sent(home, "default")
        .into_iter()
        .filter(|v| v["kind"] == "send")
        .collect();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["text"], "redacted", "{replies:?}");
    assert_eq!(replies[0]["chat"], "grp");
    assert!(
        std::fs::metadata(home.join("profiles/default/privacy.key")).is_ok(),
        "the profile's hashing key exists"
    );
    // Logs and the trajectory are written redacted too.
    for dir in ["profiles/default/logs", "logs"] {
        let leaks = written_with(&home.join(dir), "415 555 0100");
        assert!(leaks.is_empty(), "the raw phone number is in {leaks:?}");
    }
    let session = common::newest_session(home);
    let trajectory = common::ok(&common::run(
        home,
        &["trajectory", "show", &session, "--format", "jsonl"],
    ));
    assert!(trajectory.contains("what is my number"), "{trajectory}");
    assert!(trajectory.contains("not [REDACTED:phone]"), "{trajectory}");
    assert!(
        !trajectory.contains("415 555 0100") && !trajectory.contains("555-123-4567"),
        "{trajectory}"
    );
}

/// Files under `dir` containing `needle`.
fn written_with(dir: &Path, needle: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            hits.extend(written_with(&path, needle));
        } else if std::fs::read(&path).is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle)) {
            hits.push(path.display().to_string());
        }
    }
    hits
}
