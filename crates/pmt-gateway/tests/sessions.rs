//! Session methods over a real daemon: profile and ownership checks,
//! server-to-client requests, event replay, liveness and the daemon's
//! lifetime (spec: workspace-architecture, Profile and ownership checks on
//! every session method; Server-to-client requests; Event sequencing and
//! reconnect replay; Connection liveness and backpressure; Stale daemon
//! build detection; Daemon discovery file and local control channel).

mod support;

use std::time::{Duration, Instant};

use pmt_gateway::Exit;
use pmt_rpc::client::{Client, Inbox, stale_build_notice};
use pmt_rpc::contracts::{Envelope, EventsSince};
use pmt_rpc::control::{self, ControlRequest};
use serde_json::{Value, json};

use self::support::Daemon;

async fn next_event(inbox: &mut Inbox, name: &str) -> Envelope {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), inbox.events.recv())
            .await
            .expect("an event in time")
            .expect("connection open");
        if envelope.event == name {
            return envelope;
        }
    }
}

async fn create(client: &Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

async fn submit(client: &Client, session: &str, text: &str) -> Value {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap()
}

fn since(value: Value) -> EventsSince {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn another_profiles_events_are_denied_without_revealing_the_session() {
    let daemon = Daemon::start().await;
    let theirs = daemon.host.add("home");
    let (work, _inbox) = daemon.client("work", false).await;
    let replay = work
        .call(
            "session.events.since",
            json!({"session_id": theirs, "last_seen": 0}),
        )
        .await
        .unwrap_err();
    assert_eq!(replay.code, -32_002);
    let missing = work
        .call(
            "session.events.since",
            json!({"session_id": "ses_01JB7Z3Q5X8V2K4M6N8P0R2T99", "last_seen": 0}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, -32_002);
    assert_eq!(
        replay.message.replace(&theirs, "<id>"),
        missing
            .message
            .replace("ses_01JB7Z3Q5X8V2K4M6N8P0R2T99", "<id>"),
        "the same answer for another profile's session and no session"
    );
    for method in ["session.attach", "session.transcript", "session.resume"] {
        let err = work
            .call(method, json!({"session_id": theirs}))
            .await
            .unwrap_err();
        assert_eq!(err.code, -32_002, "{method}");
    }
}

#[tokio::test]
async fn changing_a_session_needs_an_attached_connection() {
    let daemon = Daemon::start().await;
    let session = daemon.host.add("default");
    let (client, _inbox) = daemon.client("default", false).await;
    let err = client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "hi"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    client
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    assert!(submit(&client, &session, "hi").await["turn_id"].is_string());
}

#[tokio::test]
async fn without_a_capable_client_an_approval_is_denied_at_once() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let started = Instant::now();
    submit(&client, &session, "approval").await;
    let finished = next_event(&mut inbox, "turn.finished").await;
    assert_eq!(finished.data["final_response"], "denied");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "not after the 30 s deadline"
    );
}

#[tokio::test]
async fn a_capable_client_answers_the_approval() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", true).await;
    let session = create(&client).await;
    submit(&client, &session, "approval").await;
    let request = tokio::time::timeout(Duration::from_secs(5), inbox.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.method, "approval.request");
    assert!(request.id.starts_with("srq-"));
    assert_eq!(request.params["subject"], "rm -rf build");
    client
        .call(
            "approval.respond",
            json!({"id": request.id, "decision": "once"}),
        )
        .await
        .unwrap();
    let finished = next_event(&mut inbox, "turn.finished").await;
    assert_eq!(finished.data["final_response"], "approved");
    let late = client
        .call(
            "approval.respond",
            json!({"id": request.id, "decision": "deny"}),
        )
        .await
        .unwrap_err();
    assert_eq!(late.code, -32_003, "an answer for a closed id is dropped");
}

#[tokio::test]
async fn events_missed_while_disconnected_are_replayed() {
    let daemon = Daemon::start().await;
    let (first, mut inbox) = daemon.client("default", false).await;
    let session = create(&first).await;
    submit(&first, &session, "steps:6:150").await;
    let seen = next_event(&mut inbox, "session.event").await;
    assert_eq!(seen.data["n"], 1);
    let watermark = seen.seq;
    let epoch = seen.epoch.clone();
    first.close();
    first.wait_closed().await;

    let (second, mut inbox) = daemon.client("default", false).await;
    second
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    next_event(&mut inbox, "turn.finished").await;
    let replay = since(
        second
            .call(
                "session.events.since",
                json!({"session_id": session, "last_seen": watermark}),
            )
            .await
            .unwrap(),
    );
    assert_eq!(replay.epoch, epoch);
    assert!(!replay.truncated);
    let steps: Vec<u64> = replay
        .events
        .iter()
        .filter(|e| e.event == "session.event")
        .map(|e| e.data["n"].as_u64().unwrap())
        .collect();
    assert_eq!(
        steps,
        [2, 3, 4, 5, 6],
        "every event after the watermark, in order"
    );
    let seqs: Vec<u64> = replay.events.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    assert_eq!(replay.latest_seq, *seqs.last().unwrap());
}

#[tokio::test]
async fn a_restarted_daemon_answers_with_a_new_epoch() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    submit(&client, &session, "steps:3:0").await;
    let finished = next_event(&mut inbox, "turn.finished").await;
    let (home, exit) = daemon.stop().await;
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(client.wait_closed().await.code, Some(1001));

    let restarted = Daemon::start_with(support::options(), home).await;
    let (client, _inbox) = restarted.client("default", false).await;
    let theirs = restarted.host.add("default");
    let replay = since(
        client
            .call(
                "session.events.since",
                json!({"session_id": theirs, "last_seen": finished.seq}),
            )
            .await
            .unwrap(),
    );
    assert_ne!(
        replay.epoch, finished.epoch,
        "the client must reset its watermark"
    );
}

#[tokio::test]
async fn ping_is_answered_during_a_long_compaction() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let compacting = {
        let client = client.clone();
        let session = session.clone();
        tokio::spawn(async move {
            client
                .call("session.compact", json!({"session_id": session}))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    client.call("rpc.ping", json!({})).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "{:?}",
        started.elapsed()
    );
    let other = client.call("session.list", json!({})).await;
    assert!(other.is_ok(), "a slow request does not block another");
    assert!(compacting.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_retiring_daemon_refuses_new_work_and_drains_running_turns() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    submit(&client, &session, "steps:3:200").await;
    let stop = ControlRequest::Stop { restart: true };
    control::call(&daemon.endpoint.control, &stop)
        .await
        .unwrap();
    let refused = client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "hi"}),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, -32_006);
    assert_eq!(
        refused.message,
        "daemon is restarting; reconnect to continue"
    );
    let finished = next_event(&mut inbox, "turn.finished").await;
    assert_eq!(
        finished.data["final_response"], "done",
        "the running turn reached its end"
    );
    assert_eq!(client.wait_closed().await.code, Some(1001));
    let (_, exit) = daemon.stop().await;
    assert_eq!(exit, Exit::Restart);
}

#[tokio::test]
async fn a_client_of_another_build_gets_the_stale_build_notice_and_still_attaches() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    assert_eq!(stale_build_notice(client.hello(), "build-a"), None);
    assert_eq!(
        stale_build_notice(client.hello(), "build-b").as_deref(),
        Some("daemon is running an older build (0.1.0); restart with 'prometheus daemon restart'")
    );
    assert!(client.call("session.list", json!({})).await.is_ok());
}

#[tokio::test]
async fn the_discovery_file_names_the_endpoint_and_goes_away_on_stop() {
    let daemon = Daemon::start().await;
    let path = daemon.home.path().join("run/gateway.json");
    let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for field in [
        "pid",
        "instance_id",
        "build_id",
        "rpc_url",
        "control",
        "rpc_protocol",
        "started_at",
    ] {
        assert!(!record[field].is_null(), "{field} missing from {record}");
    }
    assert!(
        record["rpc_url"]
            .as_str()
            .unwrap()
            .starts_with("ws://127.0.0.1:")
    );
    assert_eq!(record["rpc_protocol"], "1.0");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(std::path::Path::new(&daemon.endpoint.control)), 0o600);
    }
    let status = control::call(&daemon.endpoint.control, &ControlRequest::Status)
        .await
        .unwrap();
    let status = status.status.unwrap();
    assert_eq!(status.pid, std::process::id());
    assert_eq!(status.build_id, "build-a");
    let control_path = daemon.endpoint.control.clone();
    let (home, _) = daemon.stop().await;
    assert!(!home.path().join("run/gateway.json").exists());
    #[cfg(unix)]
    assert!(!std::path::Path::new(&control_path).exists());
    let _ = control_path;
}
