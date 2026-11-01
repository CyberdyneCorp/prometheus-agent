//! What the line interface wires into its session (spec: model-providers,
//! Reasoning effort defaults, overrides and display; skills, Skill secrets
//! and settings): `/reasoning <level>` reaches the next request, and a
//! skill's missing secret is asked for locally with the input hidden.

#![cfg(unix)]

mod common;

use std::io::{BufReader, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, newest_session, ok, run, session_messages, temp_home};
use expectrl::Expect as _;

const ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// A Chat Completions server answering `pong`, sending each request's
/// `reasoning_effort` back.
fn server() -> (String, mpsc::Receiver<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let body = common::read_http_request(&mut BufReader::new(stream.try_clone().unwrap()));
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let _ = tx.send(body["reasoning_effort"].as_str().map(str::to_owned));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ANSWER}",
                ANSWER.len()
            );
        }
    });
    (format!("http://{addr}/v1"), rx)
}

#[test]
fn a_session_reasoning_level_reaches_the_next_request() {
    let home = temp_home();
    let (url, efforts) = server();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
             [providers.acme]\nbase_url = \"{url}\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\n\
             [models.\"acme/r1\"]\nsupports_reasoning = true\nmax_reasoning_effort = \"high\"\n\n\
             [agent]\nreasoning_effort = \"low\"\n"
        ),
    )
    .unwrap();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("first").unwrap();
    p.expect("pong").unwrap();
    prompt(&mut p);
    p.send_line("/reasoning high").unwrap();
    p.expect("reasoning: high from the next request").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("pong").unwrap();
    prompt(&mut p);
    exit(p);
    let sent: Vec<Option<String>> = efforts.try_iter().collect();
    assert_eq!(
        sent,
        [Some("low".to_owned()), Some("high".to_owned())],
        "the config's level, then the session's"
    );
}

const NOTION: &str = "---\nname: notion\ndescription: Notion pages.\nmetadata:\n  prometheus:\n    secrets:\n      - name: NOTION_API_KEY\n        prompt: Notion integration token\n---\nUse the API.\n";

const SECRET_SCRIPT: &str = r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "skill_view"
arguments = { name = "notion" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "test -n \"$NOTION_API_KEY\" && echo KEY_SET || echo KEY_EMPTY" }

[[rule]]
step = 2
tool_result_regex = "KEY_SET"
text = "the secret reached the terminal"

[[rule]]
text = "the secret is missing"
"#;

/// Scenario: Secret value never reaches the transcript.
#[test]
fn a_missing_skill_secret_is_asked_for_hidden_and_never_stored_in_the_transcript() {
    let home = mock_home();
    let skill = home.path().join("profiles/default/skills/notion");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), NOTION).unwrap();
    let script = home.path().join("s.toml");
    std::fs::write(&script, SECRET_SCRIPT).unwrap();
    let mut p = spawn(
        home.path(),
        &["chat", "--yolo", "--mock-script", script.to_str().unwrap()],
    );
    prompt(&mut p);
    p.send_line("use notion").unwrap();
    p.expect("needs secret NOTION_API_KEY: Notion integration token")
        .unwrap();
    p.expect("secret › ").unwrap();
    p.send_line("ntn-hidden-value-4242").unwrap();
    p.expect("the secret reached the terminal").unwrap();
    prompt(&mut p);
    exit(p);
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(listed.contains("NOTION_API_KEY"), "{listed}");
    let transcript =
        serde_json::to_string(&session_messages(home.path(), &newest_session(home.path())))
            .unwrap();
    assert!(transcript.contains("NOTION_API_KEY"), "{transcript}");
    assert!(
        !transcript.contains("ntn-hidden-value-4242"),
        "{transcript}"
    );
    let history = std::fs::read_to_string(home.path().join("profiles/default/cli_history"))
        .unwrap_or_default();
    assert!(!history.contains("ntn-hidden-value-4242"), "{history}");
}
