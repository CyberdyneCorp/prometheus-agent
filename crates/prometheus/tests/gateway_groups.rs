//! Group chats in the real daemon with the mock provider (spec:
//! messaging-gateway, Per-chat policy thread matching and skill preload).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::gateway::{Daemon, adapter_dir, gateway_home, texts, wait};
use self::common::session_messages;

/// Drops a mentioning message of `sender` in thread `thread` of chat `grp`.
fn post(home: &Path, n: usize, thread: &str, text: &str) {
    let inbox = adapter_dir(home).join("default").join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = json!({"id": format!("g{n}"), "sender": "u1", "chat": "grp",
        "chat_type": "thread", "thread": thread, "text": text, "mentioned": true});
    let tmp = inbox.join(format!("g{n:04}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("g{n:04}.json"))).unwrap();
}

/// The cached tiers of a session's frozen system prompt (`static` and
/// `profile`; the `session` tier names the session's own start time).
fn shared_prompt(home: &Path, session: &str) -> Vec<Value> {
    let tiers: String = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT tiers FROM session_prompts WHERE session_id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap();
    let tiers: Vec<Value> = serde_json::from_str(&tiers).unwrap();
    tiers
        .into_iter()
        .filter(|t| t["tier"] != "session")
        .collect()
}

#[test]
fn a_topic_policy_preloads_its_skill_as_context_not_into_the_prompt() {
    let home = gateway_home("");
    let home = home.path();
    let skill = home.join("profiles/default/skills/release-notes/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: release-notes\ndescription: Write release notes.\n---\nGroup changes by area.\n",
    )
    .unwrap();
    common::append_config(
        home,
        "\n[[gateway.chat_policies]]\nplatform = \"test\"\nchat_id = \"grp\"\nthread_id = \"42\"\n\
         preload_skills = [\"release-notes\"]\n",
    );
    let daemon = Daemon::start(home);
    post(home, 1, "42", "bound");
    wait("the bound reply", std::time::Duration::from_mins(1), || {
        texts(home, "default").len() == 1
    });
    post(home, 2, "7", "unbound");
    wait(
        "the unbound reply",
        std::time::Duration::from_mins(1),
        || texts(home, "default").len() == 2,
    );
    daemon.stop(home);
    let listed = common::ok(&common::run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let ids: Vec<String> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
    let (bound, unbound): (Vec<&String>, Vec<&String>) = ids.iter().partition(|id| {
        session_messages(home, id).iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Group changes by area."))
        })
    });
    assert_eq!((bound.len(), unbound.len()), (1, 1));
    let first = session_messages(home, bound[0])
        .into_iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    assert!(
        first["content"]
            .as_str()
            .unwrap()
            .starts_with("# Preloaded skills"),
        "the skill opens the session: {first}"
    );
    let bound_prompt = shared_prompt(home, bound[0]);
    assert!(!bound_prompt.is_empty());
    assert_eq!(
        bound_prompt,
        shared_prompt(home, unbound[0]),
        "the cached system prompt is the same"
    );
    assert!(!bound_prompt.iter().any(|t| {
        t["text"]
            .as_str()
            .is_some_and(|text| text.contains("Group changes by area."))
    }));
}
