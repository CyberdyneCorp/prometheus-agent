//! Protocol conformance: framing, error codes, close codes, the handshake
//! and versioning, against a real daemon on a temporary home (spec:
//! workspace-architecture, Ticket-authenticated loopback attachment;
//! JSON-RPC framing and error codes; Protocol handshake and versioning;
//! Strict parameter validation; messaging-gateway, Loopback client
//! attachment; Versioned typed RPC protocol).

mod support;

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_rpc::catalog::{INDEX, IndexEntry, Kind, declare_builtin};
use pmt_rpc::client::{Client, Socket, dial, subprotocols};
use pmt_rpc::contracts::{Empty, Event, Version};
use pmt_rpc::control::Purpose;
use pmt_rpc::{Catalog, RpcError};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use self::support::{Daemon, close_code, hello, next_json, options, send};

async fn say_hello(socket: &mut Socket) -> Value {
    let params = serde_json::to_value(hello(1, 0, false)).unwrap();
    send(
        socket,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "rpc.hello", "params": params}),
    )
    .await;
    next_json(socket).await
}

#[tokio::test]
async fn a_garbage_frame_gets_32700_and_the_connection_keeps_serving() {
    let daemon = Daemon::start().await;
    let mut socket = daemon.socket("default").await;
    assert!(say_hello(&mut socket).await["result"]["epoch"].is_string());
    socket.send(Message::text("not json")).await.unwrap();
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["error"]["code"], -32_700);
    assert_eq!(reply["id"], Value::Null);
    send(
        &mut socket,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "rpc.ping"}),
    )
    .await;
    assert_eq!(next_json(&mut socket).await["result"], json!({"ok": true}));
    send(&mut socket, &json!([1])).await;
    assert_eq!(next_json(&mut socket).await["error"]["code"], -32_600);
    send(
        &mut socket,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "session.list", "params": 5}),
    )
    .await;
    assert_eq!(next_json(&mut socket).await["error"]["code"], -32_602);
}

#[tokio::test]
async fn an_unknown_ticket_closes_with_4401_and_no_frame_is_processed() {
    let daemon = Daemon::start().await;
    let mut socket = dial(&daemon.endpoint.rpc_url, &subprotocols("never-issued", 1))
        .await
        .unwrap();
    let _ = socket
        .send(Message::text(
            r#"{"jsonrpc":"2.0","id":1,"method":"rpc.ping"}"#,
        ))
        .await;
    assert_eq!(close_code(&mut socket).await, 4401);
}

#[tokio::test]
async fn a_reused_ticket_closes_with_4401() {
    let daemon = Daemon::start().await;
    let ticket = daemon.ticket("default").await;
    let protocols = subprotocols(&ticket, 1);
    let mut first = dial(&daemon.endpoint.rpc_url, &protocols).await.unwrap();
    assert!(say_hello(&mut first).await["result"].is_object());
    let mut second = dial(&daemon.endpoint.rpc_url, &protocols).await.unwrap();
    assert_eq!(close_code(&mut second).await, 4401);
}

#[tokio::test]
async fn a_browser_origin_closes_with_4403() {
    let daemon = Daemon::start().await;
    let ticket = daemon.ticket("default").await;
    let mut request = daemon
        .endpoint
        .rpc_url
        .as_str()
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&subprotocols(&ticket, 1)).unwrap(),
    );
    headers.insert("Origin", HeaderValue::from_static("https://evil.example"));
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(close_code(&mut socket).await, 4403);
}

#[tokio::test]
async fn a_request_before_hello_needs_the_handshake() {
    let daemon = Daemon::start().await;
    let mut socket = daemon.socket("default").await;
    send(
        &mut socket,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "session.list"}),
    )
    .await;
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["error"]["code"], -32_008);
    assert_eq!(reply["error"]["message"], "handshake required");
}

#[tokio::test]
async fn a_major_mismatch_fails_naming_both_versions_then_closes_4426() {
    let mut options = options();
    options.catalog =
        Some(Catalog::build(Version { major: 2, minor: 0 }, INDEX, declare_builtin).unwrap());
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    let ticket = daemon.ticket("default").await;
    let mut socket = dial(&daemon.endpoint.rpc_url, &subprotocols(&ticket, 1))
        .await
        .unwrap();
    let params = serde_json::to_value(hello(1, 7, false)).unwrap();
    send(
        &mut socket,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "rpc.hello", "params": params}),
    )
    .await;
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["error"]["code"], -32_008);
    assert_eq!(
        reply["error"]["message"],
        "protocol mismatch: client 1.7, daemon 2.0; run 'prometheus update'"
    );
    assert_eq!(close_code(&mut socket).await, 4426);
}

/// An event introduced in minor 3.
struct Newer;

impl Event for Newer {
    const NAME: &'static str = "test.newer";
    type Payload = Empty;
}

#[tokio::test]
async fn an_older_minor_client_attaches_and_gets_nothing_newer() {
    let mut index = INDEX.to_vec();
    index.push(IndexEntry {
        name: "test.newer",
        kind: Kind::Event,
        owner: "workspace-architecture",
        since: 3,
        long: false,
    });
    let builtin = Catalog::builtin();
    let catalog = Catalog::build(Version { major: 1, minor: 4 }, &index, |d| {
        declare_builtin(d);
        d.event::<Newer>();
    })
    .unwrap();
    assert_eq!(builtin.protocol().major, 1);
    let mut options = options();
    options.catalog = Some(catalog);
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    let (client, mut inbox) = Client::connect(
        &daemon.endpoint,
        Purpose::Interactive,
        vec!["default".into()],
        &hello(1, 2, false),
    )
    .await
    .expect("a lower minor of the same major is accepted");
    assert_eq!(client.hello().protocol, Version { major: 1, minor: 4 });
    assert!(
        !client
            .hello()
            .capabilities
            .events
            .contains(&"test.newer".to_owned())
    );
    let session = daemon.host.add("default");
    client
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    daemon
        .server
        .hub()
        .publish(&session, "test.newer", json!({}));
    daemon
        .server
        .hub()
        .publish(&session, "session.event", json!({"type": "x"}));
    let first = tokio::time::timeout(Duration::from_secs(5), inbox.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.event, "session.event",
        "the 1.3 event was not sent to a 1.2 client"
    );
}

#[tokio::test]
async fn undeclared_and_unserved_methods_have_their_codes() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let err: RpcError = client
        .call("session.frobnicate", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_601);
    assert_eq!(err.message, "unknown method: session.frobnicate");
    let err = client.call("evolve.list", json!({})).await.unwrap_err();
    assert_eq!(err.code, -32_006);
    assert_eq!(err.data.unwrap().code, "rpc.not_served");
}

#[tokio::test]
async fn params_are_validated_before_any_handler_runs() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let err = client
        .call(
            "session.resume",
            json!({"sesion_id": "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    let data = err.data.unwrap();
    assert_eq!(data.path.as_deref(), Some("/sesion_id"));
    assert_eq!(data.code, "rpc.unknown_field");
    let err = client
        .call("prompt.submit", json!({"text": "hi"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    assert_eq!(err.data.unwrap().path.as_deref(), Some("/session_id"));
}

#[tokio::test]
async fn an_oversized_frame_closes_with_4413() {
    let mut options = options();
    options.rpc.max_frame_bytes = 2048;
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    let mut socket = daemon.socket("default").await;
    assert!(say_hello(&mut socket).await["result"].is_object());
    let big = json!({"jsonrpc": "2.0", "id": 2, "method": "rpc.ping", "params": {"pad": "x".repeat(4096)}});
    let _ = socket.send(Message::text(big.to_string())).await;
    assert_eq!(close_code(&mut socket).await, 4413);
}

#[tokio::test]
async fn a_budget_refusal_is_32005_with_its_kind() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let created = client.call("session.create", json!({})).await.unwrap();
    let err = client
        .call(
            "prompt.submit",
            json!({"session_id": created["session_id"], "text": "budget"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_005);
    assert_eq!(err.kind(), Some("budget_exceeded"));
}

#[tokio::test]
async fn a_silent_connection_closes_with_4408_after_45_seconds() {
    let daemon = Daemon::start().await;
    let mut socket = daemon.socket("default").await;
    // Before the hello: the daemon's idle deadline counts from its last
    // frame, which is the hello's answer.
    let started = tokio::time::Instant::now();
    assert!(say_hello(&mut socket).await["result"].is_object());
    // From here the clock jumps whenever every task waits.
    tokio::time::pause();
    // One-second steps: a single long timeout could let the paused clock
    // jump past it while the close frame is still in the socket.
    let close = loop {
        match tokio::time::timeout(Duration::from_secs(1), socket.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => break u16::from(frame.code),
            Ok(Some(Ok(_))) => {}
            Err(_) if started.elapsed() < Duration::from_mins(2) => {}
            other => panic!("no close frame: {other:?}"),
        }
    };
    assert_eq!(close, 4408);
    assert!(started.elapsed() >= Duration::from_secs(45));
}
