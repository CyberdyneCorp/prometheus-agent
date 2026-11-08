//! The context window view, the context pressure gauge on the CLI and
//! server-side Responses compaction through the real binary (spec:
//! context-compression, Context window view; Compaction progress notices
//! and context pressure gauge; Server-side Responses compaction (opt-in)).

#![cfg(unix)]

mod common;

use std::io::{BufReader, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use common::pty::{exit, prompt, spawn};
use common::{append_config, mock_home, newest_session, ok, run, stderr, temp_home};
use expectrl::{Expect as _, Regex};

/// Model calls recorded so far in the profile.
fn calls(home: &std::path::Path) -> u64 {
    let out = ok(&run(home, &["--json", "usage", "--by", "day"]));
    let usage: serde_json::Value = serde_json::from_str(&out).unwrap();
    usage["total"]["calls"].as_u64().unwrap_or(0)
}

/// Scenario: Why is my file ignored.
#[test]
fn context_shows_the_loaded_and_the_shadowed_file_without_a_model_call() {
    let home = mock_home();
    let repo = home.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".prometheus.md"), "Use tabs.\n").unwrap();
    std::fs::write(repo.join("CLAUDE.md"), "Use spaces.\n").unwrap();
    let repo = repo.to_string_lossy().into_owned();
    let mut p = spawn(home.path(), &["chat", "--cwd", &repo]);
    prompt(&mut p);
    p.send_line("/context").unwrap();
    p.expect("Context window: ").unwrap();
    p.expect(Regex(r"\.prometheus\.md: \d+ tokens, loaded"))
        .unwrap();
    p.expect(Regex(r"CLAUDE\.md: \d+ tokens, shadowed"))
        .unwrap();
    prompt(&mut p);
    p.send_line("/ctx all").unwrap();
    p.expect("Toolsets:").unwrap();
    prompt(&mut p);
    exit(p);
    assert_eq!(calls(home.path()), 0, "no model call");
}

/// Scenario: Gauge crossing (on the CLI): a one-shot whose context ends
/// past 60% of the compaction trigger prints the info-level gauge.
#[test]
fn a_turn_past_sixty_percent_of_the_trigger_prints_the_gauge() {
    let home = mock_home();
    // A 40K window: the trigger is about 20K tokens.
    append_config(
        home.path(),
        "\n[models.\"mock/mock\"]\ncontext_window = 40000\n",
    );
    let prompt_text = "lots of words ".repeat(4_200);
    let out = run(home.path(), &["-z", &prompt_text]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let gauge = err
        .lines()
        .find(|l| l.starts_with("context ") && l.contains("% to compaction"))
        .unwrap_or_else(|| panic!("no gauge in {err}"));
    let percent: u64 = gauge
        .split_whitespace()
        .nth(2)
        .and_then(|p| p.trim_end_matches('%').parse().ok())
        .unwrap();
    assert!(percent >= 60, "{gauge}");
    assert_eq!(gauge.chars().filter(|c| *c == '█' || *c == '░').count(), 20);
}

/// One Responses stream: an optional compaction item, then `text`.
fn responses_stream(item: bool, text: &str) -> String {
    let compaction = if item {
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"id\":\"cmp_1\",\"encrypted_content\":\"enc_ABC123\"}}\n\n"
    } else {
        ""
    };
    format!(
        "{compaction}event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"{text}\"}}\n\n\
         event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":50,\"output_tokens\":2}}}}}}\n\n"
    )
}

/// A Responses server whose first answer carries a compaction item; every
/// request body is sent back.
fn server() -> (String, mpsc::Receiver<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (n, mut stream) in listener.incoming().flatten().enumerate() {
            let body = common::read_http_request(&mut BufReader::new(stream.try_clone().unwrap()));
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or_default());
            let answer = responses_stream(n == 0, &format!("answer {n}"));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (format!("http://{addr}/v1"), rx)
}

/// Scenario: Compaction item replayed.
#[test]
fn the_compaction_item_is_replayed_and_the_transcript_keeps_every_message() {
    let home = temp_home();
    let (url, bodies) = server();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
             [providers.acme]\nbase_url = \"{url}\"\nwire_format = \"responses\"\nauth = \"none\"\n\
             supports_server_compaction = true\n\n\
             [compression]\nresponses_native = true\n\n[budgets]\nallow_unpriced = true\n"
        ),
    )
    .unwrap();
    ok(&run(home.path(), &["-z", "first question"]));
    let session = newest_session(home.path());
    ok(&run(
        home.path(),
        &["-z", "second question", "--resume", &session],
    ));

    let sent: Vec<serde_json::Value> = bodies.try_iter().collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[0]["context_management"][0]["compact_threshold"]
            .as_u64()
            .unwrap()
            > 0
    );
    let input = sent[1]["input"].to_string();
    assert_eq!(
        sent[1]["input"][0]["encrypted_content"], "enc_ABC123",
        "{input}"
    );
    assert!(!input.contains("first question"), "{input}");
    assert!(input.contains("second question"), "{input}");
    // The stored transcript keeps all four messages: `sessions show`
    // counts them and the export lists them.
    let shown = ok(&run(home.path(), &["sessions", "show", &session]));
    assert!(
        shown
            .lines()
            .any(|l| l.starts_with("messages") && l.ends_with(" 4")),
        "{shown}"
    );
    let stored: Vec<String> = common::session_messages(home.path(), &session)
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    for text in ["first question", "answer 0", "second question", "answer 1"] {
        assert!(
            stored.iter().any(|t| t == text),
            "{text} missing from {stored:?}"
        );
    }
}
