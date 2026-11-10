//! One trajectory grammar for every surface (spec: observability, One
//! trajectory grammar for every surface — Same script, interactive and
//! one-shot): the same mock-provider conversation run through the
//! interactive line interface and through `chat -z` records the same event
//! sequence, apart from ids, timestamps and `payload.source`.

#![cfg(unix)]

mod common;

use std::path::Path;

use common::pty::{exit, prompt, spawn};
use expectrl::Expect as _;
use serde_json::Value;

/// One tool call, then the answer.
const SCRIPT: &str = r#"
[[rule]]
step = 0
text = "Let me look."
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "notes.txt" }

[[rule]]
step = 1
tool_result = "remember the milk"
text = "The note says: remember the milk."
"#;

const PROMPT: &str = "what does the note say?";

/// The events of session `id`, without what may differ between two runs:
/// ids, timestamps and durations, `payload.source`, and what the surface's
/// system prompt changes (prompt and request hashes, token counts and the
/// `turn.end` context size: each interface has its own platform hint).
fn normalized(home: &Path, id: &str) -> Vec<Value> {
    common::ok(&common::run(
        home,
        &["trajectory", "show", id, "--format", "jsonl"],
    ))
    .lines()
    .map(|line| {
        let mut event: Value = serde_json::from_str(line).unwrap();
        if let Some(payload) = event["payload"].as_object_mut() {
            payload.remove("source");
        }
        strip(&mut event);
        event
    })
    .collect()
}

fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| {
                !(key == "id"
                    || key == "ts"
                    || key.ends_with("_id")
                    || key.ends_with("_at")
                    || key.ends_with("_ms")
                    || key.ends_with("_hash")
                    || key == "tokens"
                    || key == "context_tokens")
            });
            map.values_mut().for_each(strip);
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// Both sessions run in one home, from the same directory.
#[test]
fn the_interactive_and_one_shot_trajectories_match() {
    let home = common::mock_home();
    std::fs::write(home.path().join("notes.txt"), "remember the milk\n").unwrap();
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let script = script.to_str().unwrap();

    let out = common::command(home.path())
        .current_dir(home.path())
        .args(["chat", "-z", PROMPT, "--mock-script", script])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        common::stdout(&out).contains("remember the milk"),
        "{}",
        common::stderr(&out)
    );
    let one_shot = common::newest_session(home.path());

    let mut p = spawn(home.path(), &["--mock-script", script]);
    prompt(&mut p);
    p.send_line(PROMPT).unwrap();
    p.expect("The note says").unwrap();
    prompt(&mut p);
    exit(p);
    let interactive = common::newest_session(home.path());
    assert_ne!(one_shot, interactive);

    let (a, b) = (
        normalized(home.path(), &one_shot),
        normalized(home.path(), &interactive),
    );
    assert!(a.len() > 3, "{a:#?}");
    assert_eq!(a, b);
}
