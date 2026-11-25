//! Task 81.3, Bot Mode: a group chat of two mock bots, driven from a
//! desktop connection to the daemon (the desktop's Bot Mode calls), settles
//! by itself (spec: bot-mode, Group chats; Durable room driver). The
//! first post gets one answer and settles once a round goes silent; a
//! post nobody answers settles after its first round, without a second.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::bots::{self, admin_call, bot, wait};
use crate::common::daemon;

fn call(home: &Path, method: &str, params: &Value) -> Value {
    admin_call(home, method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
}

/// The entries of the room's log after its `settled` entry number `n`
/// (1-based) was written.
fn settled(home: &Path, room: &str, n: usize) -> Vec<Value> {
    let mut entries = Vec::new();
    wait("the room settles", Duration::from_mins(2), || {
        let log = call(home, "groups.log", &json!({"room_id": room}));
        entries.clone_from(log["entries"].as_array().unwrap());
        entries.iter().filter(|e| e["kind"] == "settled").count() >= n
    });
    entries
}

fn of<'a>(entries: &'a [Value], kind: &str) -> Vec<&'a Value> {
    entries.iter().filter(|e| e["kind"] == kind).collect()
}

#[test]
fn a_bot_group_chat_on_the_desktop_settles_after_a_silent_round() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(
        home.path(),
        "ann",
        "[[rule]]\nuser_regex = 'what do you think'\ntext = \"ship it on Friday\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"[PASS]\"\n",
        &[],
    );
    bot(
        home.path(),
        "ben",
        "[[rule]]\nrepeat = true\ntext = \"[PASS]\"\n",
        &[],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let room = call(
        home.path(),
        "groups.create",
        &json!({"name": "Release", "members": ["ann", "ben"]}),
    );
    let room = room["room_id"].as_str().unwrap().to_owned();

    call(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "what do you think about the release?"}),
    );
    let first = settled(home.path(), &room, 1);
    let said: Vec<&str> = of(&first, "message")
        .iter()
        .filter(|e| e["author"] == "ann")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert!(said.contains(&"ship it on Friday"), "{first:?}");
    assert_eq!(of(&first, "settled")[0]["data"]["rounds"], 2, "{first:?}");

    call(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "anything else?"}),
    );
    let second = settled(home.path(), &room, 2);
    let tail = &second[first.len()..];
    assert_eq!(of(tail, "round").len(), 1, "{tail:?}");
    assert_eq!(of(tail, "pass").len(), 2, "{tail:?}");
    assert_eq!(of(tail, "settled")[0]["data"]["rounds"], 1, "{tail:?}");
}
