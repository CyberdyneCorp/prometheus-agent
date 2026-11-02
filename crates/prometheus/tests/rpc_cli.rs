//! `prometheus rpc call` and `prometheus rpc events` against a real daemon
//! (spec: workspace-architecture, RPC debug client).

#![cfg(feature = "gateway")]

mod common;

use std::io::BufRead as _;
use std::process::Stdio;

use serde_json::{Value, json};

use self::common::{daemon, mock_home, ok, run, stderr, temp_home};

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[test]
fn call_performs_the_handshake_and_a_session_method() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let hello = json_of(&ok(&run(home.path(), &["rpc", "call", "rpc.hello"])));
    assert_eq!(hello["profiles"], json!(["default"]));
    assert_eq!(hello["protocol"]["major"], 1);
    assert!(
        hello["capabilities"]["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("session.list"))
    );

    let created = json_of(&ok(&run(
        home.path(),
        &["rpc", "call", "session.create", "{}"],
    )));
    let id = created["session_id"].as_str().unwrap();
    let listed = json_of(&ok(&run(
        home.path(),
        &["rpc", "call", "session.list", "{\"limit\": 5}"],
    )));
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id), "{listed}");
}

/// Scenario: Calling an unknown method.
#[test]
fn an_unknown_method_prints_the_32601_error_and_exits_2() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(home.path(), &["rpc", "call", "sesion.list"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("-32601"), "{err}");
    assert!(err.contains("unknown method: sesion.list"), "{err}");
    assert!(err.contains("did you mean 'session.list'"), "{err}");
}

#[test]
fn an_error_exits_with_the_code_of_its_kind() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(
        home.path(),
        &["rpc", "call", "session.resume", "{\"sesion_id\": \"x\"}"],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("\"path\": \"/sesion_id\""),
        "{}",
        stderr(&out)
    );
    let out = run(home.path(), &["rpc", "call", "session.list", "not json"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn without_a_daemon_call_exits_69() {
    let home = temp_home();
    let out = run(home.path(), &["rpc", "call", "session.list"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("prometheus daemon start"),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn events_streams_json_lines_until_interrupted() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let session = runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let created = client.call("session.create", json!({})).await.unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        client
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "hi"}),
            )
            .await
            .unwrap();
        while let Some(event) = inbox.events.recv().await {
            if event.event == "turn.finished" {
                break;
            }
        }
        session
    });
    let mut child = common::command(home.path())
        .args(["rpc", "events", "--session", &session, "--since", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seqs = Vec::new();
    loop {
        let line = lines.next().expect("an event line").unwrap();
        let event = json_of(&line);
        assert_eq!(event["session_id"], session.as_str());
        seqs.push(event["seq"].as_u64().unwrap());
        if event["event"] == "turn.finished" {
            assert_eq!(event["data"]["final_response"], "mock: hi");
            break;
        }
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
}
