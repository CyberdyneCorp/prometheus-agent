//! Session affinity in a daemon (spec: model-providers, Session affinity
//! headers and per-model wire routing): the daemon serves every session of
//! a profile and working directory from one shared factory, yet each
//! session's requests carry the affinity value of its own conversation,
//! and opening another session never changes it.

#![cfg(feature = "gateway")]

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{daemon, ok, run, run_with_stdin, temp_home};
use serde_json::json;

/// One request the fake endpoint received: its affinity header and body.
#[derive(Debug, Clone)]
struct Seen {
    affinity: Option<String>,
    body: String,
}

/// A Chat Completions endpoint answering `ok` and recording every request.
fn endpoint() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (affinity, length) = headers(&mut reader);
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            record.lock().unwrap().push(Seen {
                affinity,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            let chunk = json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}]});
            let reply = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (format!("http://{addr}/zen/v1"), seen)
}

/// The `x-opencode-session` header and the content length.
fn headers(reader: &mut impl BufRead) -> (Option<String>, usize) {
    let (mut affinity, mut length) = (None, 0);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            return (affinity, length);
        }
        let (name, value) = line.split_once(':').unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "x-opencode-session" => affinity = Some(value.trim().to_owned()),
            "content-length" => length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
}

async fn create(client: &pmt_rpc::client::Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

async fn turn(
    client: &pmt_rpc::client::Client,
    inbox: &mut pmt_rpc::client::Inbox,
    id: &str,
    text: &str,
) {
    client
        .call("prompt.submit", json!({"session_id": id, "text": text}))
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("a turn finishes within 30 s")
            .expect("the connection is open");
        if event.event == "turn.finished" && event.session_id.as_deref() == Some(id) {
            return;
        }
    }
}

/// Scenario: Affinity header stable within a conversation.
#[test]
fn daemon_sessions_sharing_a_factory_keep_their_own_affinity_value() {
    let home = temp_home();
    let (url, seen) = endpoint();
    ok(&run(home.path(), &["config", "path"]));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENCODE_ZEN_API_KEY"],
        "zen-key-0123456789\n",
    ));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"opencode-zen\"\ndefault = \"glm-5\"\n\n\
             [providers.opencode-zen]\nbase_url = \"{url}\"\nkey_hosts = [\"127.0.0.1\"]\n\n\
             [budgets]\nallow_unpriced = true\n"
        ),
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let first = create(&client).await;
        turn(&client, &mut inbox, &first, "marker-alpha one").await;
        // Same profile, cwd and setup: the daemon reuses the factory.
        let second = create(&client).await;
        turn(&client, &mut inbox, &second, "marker-beta one").await;
        turn(&client, &mut inbox, &first, "marker-alpha two").await;
    });

    // Every request of a session, main or auxiliary, by its marker.
    let mut values: BTreeMap<&str, Vec<Option<String>>> = BTreeMap::new();
    let seen = seen.lock().unwrap().clone();
    for request in &seen {
        for marker in ["marker-alpha", "marker-beta"] {
            if request.body.contains(marker) {
                values
                    .entry(marker)
                    .or_default()
                    .push(request.affinity.clone());
            }
        }
    }
    let alpha = &values["marker-alpha"];
    let beta = &values["marker-beta"];
    assert!(alpha.len() >= 2 && !beta.is_empty(), "{seen:?}");
    assert!(alpha[0].is_some(), "{seen:?}");
    assert!(
        alpha.iter().all(|v| *v == alpha[0]),
        "the first session kept its value after the second opened: {alpha:?}"
    );
    assert!(beta.iter().all(|v| *v == beta[0]), "{beta:?}");
    assert_ne!(alpha[0], beta[0], "another session, another value");
}
