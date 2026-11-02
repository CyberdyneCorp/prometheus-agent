//! The client-protocol conformance suite of the `tui` capability: the
//! session-attachment semantics every interactive client relies on,
//! against a real daemon on a temporary home (spec: tui, Hello handshake
//! is mandatory and versioned; Typed method registry is enforced; Error
//! code and close code taxonomy; Session access requires an attachment;
//! Long requests do not block the connection; Server-to-client requests;
//! Server request deadlines fail closed; Idempotent session creation;
//! Prompt submission while busy; Turn authors are stamped by the gateway;
//! Profile scope on every request; Discovery RPCs preserve the prompt
//! cache; Composer and path completion; Orphaned attachments are reaped).

mod support;

use std::time::Duration;

use pmt_rpc::client::{Client, Inbox, Socket, dial, subprotocols};
use pmt_rpc::contracts::{ClientKind, Envelope};
use pmt_rpc::control::Purpose;
use serde_json::{Value, json};

use self::support::{Daemon, close_code, hello, next_json, options, send};

async fn next_event(inbox: &mut Inbox, name: &str) -> Envelope {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), inbox.events.recv())
            .await
            .expect("an event within 5 s")
            .expect("the connection is open");
        if event.event == name {
            return event;
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

fn data_code(err: &pmt_rpc::RpcError) -> &str {
    err.data.as_ref().map_or("", |d| d.code.as_str())
}

#[tokio::test]
async fn a_first_request_other_than_hello_creates_nothing() {
    let daemon = Daemon::start().await;
    let mut socket = daemon.socket("default").await;
    send(
        &mut socket,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "session.create", "params": {}}),
    )
    .await;
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["error"]["code"], -32_008);
    assert_eq!(daemon.server.status().sessions, 0, "no session was created");
}

#[tokio::test]
async fn a_misspelled_prompt_param_names_it_and_the_update_command() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let err = client
        .call(
            "prompt.submit",
            json!({"session_id": session, "sesion_id": session, "text": "hi"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    assert!(err.message.contains("sesion_id"), "{}", err.message);
    assert!(err.message.contains("prometheus update"), "{}", err.message);
    assert!(daemon.host.started.lock().unwrap().is_empty());
}

/// Upgrades to `path` on the daemon's listener with a valid ticket.
async fn upgrade_to(daemon: &Daemon, path: &str) -> Socket {
    let ticket = daemon.ticket("default").await;
    let url = daemon.endpoint.rpc_url.replace("/rpc", path);
    dial(&url, &subprotocols(&ticket, 1)).await.unwrap()
}

#[tokio::test]
async fn an_endpoint_this_surface_does_not_expose_closes_4404_despite_a_valid_ticket() {
    let daemon = Daemon::start().await;
    let mut socket = upgrade_to(&daemon, "/display").await;
    assert_eq!(close_code(&mut socket).await, 4404);
    // The ticket was never redeemed: it still opens the real endpoint once.
    let ticket = daemon.ticket("default").await;
    let mut other = upgrade_to(&daemon, "/dashboard").await;
    assert_eq!(close_code(&mut other).await, 4404);
    let mut ok = dial(&daemon.endpoint.rpc_url, &subprotocols(&ticket, 1))
        .await
        .unwrap();
    let params = serde_json::to_value(hello(1, 0, false)).unwrap();
    send(
        &mut ok,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "rpc.hello", "params": params}),
    )
    .await;
    assert!(next_json(&mut ok).await["result"].is_object());
}

#[tokio::test]
async fn another_profiles_events_are_refused_and_none_returned() {
    let daemon = Daemon::start().await;
    let personal = daemon.host.add("personal");
    let (work, _inbox) = daemon.client("work", false).await;
    let err = work
        .call(
            "session.events.since",
            json!({"session_id": personal, "last_seen": 0}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    assert!(err.data.is_some(), "an error, never an event list");
    let attach = work
        .call("session.attach", json!({"session_id": personal}))
        .await
        .unwrap_err();
    assert_eq!(attach.code, -32_002, "nor can it attach to it");
}

#[tokio::test]
async fn a_session_never_attached_is_refused_with_session_not_attached() {
    let daemon = Daemon::start().await;
    let (owner, _owner_inbox) = daemon.client("default", false).await;
    let session = create(&owner).await;
    let (guesser, _inbox) = daemon.client("default", false).await;
    for method in [
        "subagent.list",
        "session.transcript",
        "commands.catalog",
        "model.options",
    ] {
        let err = guesser
            .call(method, json!({"session_id": session}))
            .await
            .unwrap_err();
        assert_eq!(err.code, -32_002, "{method}");
        assert_eq!(data_code(&err), "session.not_attached", "{method}");
        assert_eq!(
            err.message,
            format!("session {session} is not attached on this connection")
        );
    }
    guesser
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    let listed = guesser
        .call("subagent.list", json!({"session_id": session}))
        .await
        .unwrap();
    assert_eq!(listed, json!({"subagents": []}));
}

#[tokio::test]
async fn a_catalog_request_is_not_held_behind_a_slow_resume() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let resuming = {
        let client = client.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            client
                .call(
                    "session.resume",
                    json!({"session_id": session, "setup": {"model": "slow"}}),
                )
                .await
                .unwrap();
            started.elapsed()
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = tokio::time::Instant::now();
    let catalog = client
        .call("commands.catalog", json!({"session_id": session}))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "answered first");
    assert!(
        catalog["commands"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    );
    assert!(
        resuming.await.unwrap() >= Duration::from_secs(2),
        "the resume still completed"
    );
}

#[tokio::test]
async fn a_panicking_long_request_answers_32099_and_the_connection_stays() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let err = client
        .call(
            "command.dispatch",
            json!({"session_id": session, "name": "panic"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_099);
    assert_eq!(err.message, "internal error: command.dispatch");
    let echoed = client
        .call(
            "command.dispatch",
            json!({"session_id": session, "name": "echo", "args": "still here"}),
        )
        .await
        .unwrap();
    assert_eq!(echoed["notice"], "still here");
}

#[tokio::test]
async fn the_other_window_hears_which_client_answered() {
    let daemon = Daemon::start().await;
    let (tui, mut tui_inbox) = daemon.client_as(ClientKind::Tui, "default", true).await;
    let session = create(&tui).await;
    let (desktop, mut desktop_inbox) = daemon.client_as(ClientKind::Desktop, "default", true).await;
    desktop
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    submit(&tui, &session, "approval").await;
    let card = desktop_inbox.requests.recv().await.unwrap();
    let asked = tui_inbox.requests.recv().await.unwrap();
    assert_eq!(card.id, asked.id, "both windows show the same approval");
    tui.call(
        "approval.respond",
        json!({"id": asked.id, "decision": "once"}),
    )
    .await
    .unwrap();
    let resolved = next_event(&mut desktop_inbox, "request.resolved").await;
    assert_eq!(resolved.data["id"], card.id.as_str());
    let by = resolved.data["by_client"].as_str().unwrap();
    assert!(by.starts_with("tui-"), "names the TUI client: {by}");
    let finished = next_event(&mut tui_inbox, "turn.finished").await;
    assert_eq!(finished.data["final_response"], "approved");
}

#[tokio::test]
async fn without_a_capable_driver_the_request_settles_as_no_responder() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    submit(&client, &session, "approval").await;
    let cancel = tokio::time::timeout(
        Duration::from_secs(2),
        next_event(&mut inbox, "request.cancel"),
    )
    .await
    .expect("settled at once, not at the 30 s deadline");
    assert_eq!(cancel.data["reason"], "no_responder");
    let finished = next_event(&mut inbox, "turn.finished").await;
    assert_eq!(finished.data["final_response"], "denied");
}

#[tokio::test]
async fn a_late_answer_is_32004_and_the_call_stays_denied() {
    let daemon = Daemon::start().await;
    let (client, mut inbox) = daemon.client("default", true).await;
    let session = create(&client).await;
    submit(&client, &session, "approval-fast").await;
    let request = inbox.requests.recv().await.unwrap();
    assert!(
        request.params["deadline_ms"]
            .as_u64()
            .is_some_and(|ms| ms <= 200)
    );
    let cancel = next_event(&mut inbox, "request.cancel").await;
    assert_eq!(cancel.data["reason"], "timeout");
    let finished = next_event(&mut inbox, "turn.finished").await;
    assert_eq!(finished.data["final_response"], "denied");
    let late = client
        .call(
            "approval.respond",
            json!({"id": request.id, "decision": "once"}),
        )
        .await
        .unwrap_err();
    assert_eq!(late.code, -32_004);
    assert_eq!(
        late.message,
        format!("request {} already settled", request.id)
    );
}

#[tokio::test]
async fn a_retried_create_with_the_same_key_returns_the_original_session() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let params = json!({"idempotency_key": "create-7f3a"});
    let first = client.call("session.create", params.clone()).await.unwrap();
    // The response was lost: a new connection retries.
    let (retry, _inbox) = daemon.client("default", false).await;
    let second = retry.call("session.create", params).await.unwrap();
    assert_eq!(first["session_id"], second["session_id"]);
    assert_eq!(daemon.server.status().sessions, 1, "one session row");
    let other = retry
        .call("session.create", json!({"idempotency_key": "create-other"}))
        .await
        .unwrap();
    assert_ne!(other["session_id"], first["session_id"]);
}

#[tokio::test]
async fn a_create_claiming_another_client_kind_is_refused() {
    let daemon = Daemon::start().await;
    let (dashboard, _inbox) = daemon
        .client_as(ClientKind::Dashboard, "default", false)
        .await;
    let err = dashboard
        .call("session.create", json!({"source": "tui"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    assert_eq!(data_code(&err), "rpc.source_mismatch");
    assert_eq!(daemon.server.status().sessions, 0);
    dashboard
        .call("session.create", json!({"source": "dashboard"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_resent_input_gets_its_original_admission_and_runs_once() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let params = json!({"session_id": session, "input_id": "in-1", "text": "steps:1:0"});
    let first = client.call("prompt.submit", params.clone()).await.unwrap();
    let again = client.call("prompt.submit", params).await.unwrap();
    assert_eq!(first, again, "the original admission");
    assert_eq!(daemon.host.started.lock().unwrap().len(), 1, "one turn");
}

#[tokio::test]
async fn a_client_supplied_author_is_refused_and_inputs_are_stamped() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client_as(ClientKind::Tui, "default", false).await;
    let session = create(&client).await;
    let err = client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "hi", "author": {"client_kind": "desktop"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
    assert_eq!(data_code(&err), "rpc.forged_author");
    assert_eq!(
        err.message,
        "turn author is stamped by the gateway, never by a client"
    );
    assert!(daemon.host.started.lock().unwrap().is_empty(), "no turn");
    submit(&client, &session, "hi").await;
    let authors = daemon.host.authors.lock().unwrap().clone();
    assert_eq!(authors.len(), 1);
    assert_eq!(authors[0].client_kind, ClientKind::Tui);
    assert!(authors[0].client_id.starts_with("tui-"));
    assert_eq!(authors[0].principal, "default");
}

#[tokio::test]
async fn a_connection_granted_two_profiles_must_name_one() {
    let daemon = Daemon::start().await;
    let ticket = daemon
        .ticket_for(Purpose::ProfileAdmin, &["default", "work"])
        .await;
    let socket = dial(&daemon.endpoint.rpc_url, &subprotocols(&ticket, 1))
        .await
        .unwrap();
    let (client, _inbox) = Client::over(socket, &hello(1, 0, false)).await.unwrap();
    assert_eq!(client.hello().profiles, ["default", "work"]);
    let err = client
        .call("config.get", json!({"key": "model.default"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    assert_eq!(
        err.message,
        "profile is required: this connection is granted several profiles"
    );
    assert!(
        daemon.host.config_reads.lock().unwrap().is_empty(),
        "no profile's config was read"
    );
    let named = client
        .call(
            "config.get",
            json!({"key": "model.default", "profile": "work"}),
        )
        .await
        .unwrap();
    assert_eq!(named["value"], "mock/m1");
    assert_eq!(*daemon.host.config_reads.lock().unwrap(), ["work"]);
    let foreign = client
        .call(
            "config.get",
            json!({"key": "model.default", "profile": "home"}),
        )
        .await
        .unwrap_err();
    assert_eq!(data_code(&foreign), "profile.not_granted");
    assert_eq!(
        foreign.message,
        "profile home is not granted to this connection"
    );
}

#[tokio::test]
async fn path_completion_lists_the_sessions_backend_and_an_unreachable_one_is_empty() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let listed = client
        .call(
            "complete.path",
            json!({"session_id": session, "prefix": "./src/"}),
        )
        .await
        .unwrap();
    assert_eq!(
        listed,
        json!({"items": [
            {"text": "./src/main.rs", "description": null, "dir": false},
            {"text": "./src/lib/", "description": null, "dir": true},
        ]})
    );
    let unreachable = client
        .call(
            "complete.path",
            json!({"session_id": session, "prefix": "/unreachable/x"}),
        )
        .await
        .unwrap();
    assert_eq!(unreachable, json!({"items": []}), "empty, no error");
}

#[tokio::test]
async fn slash_completion_offers_runnable_commands_first_and_never_hidden_ones() {
    let daemon = Daemon::start().await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    let listed = client
        .call(
            "complete.slash",
            json!({"session_id": session, "prefix": "/"}),
        )
        .await
        .unwrap();
    let names: Vec<&str> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["text"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["/compact", "/compress", "/deploy", "/snapshot"]);
    let narrowed = client
        .call(
            "complete.slash",
            json!({"session_id": session, "prefix": "/comp"}),
        )
        .await
        .unwrap();
    assert_eq!(narrowed["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_dropped_driver_leaves_its_turn_running_and_approvals_settle_no_responder() {
    let daemon = Daemon::start().await;
    let (tui, mut inbox) = daemon.client("default", true).await;
    let session = create(&tui).await;
    submit(&tui, &session, "approval").await;
    let asked = inbox.requests.recv().await.unwrap();
    tui.close();
    let (back, _back_inbox) = daemon.client("default", false).await;
    back.call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    let mut settled = None;
    for _ in 0..100 {
        let replay = back
            .call(
                "session.events.since",
                json!({"session_id": session, "last_seen": 0}),
            )
            .await
            .unwrap();
        let events = replay["events"].as_array().unwrap().clone();
        let cancel = events
            .iter()
            .find(|e| e["event"] == "request.cancel")
            .cloned();
        let finished = events.iter().any(|e| e["event"] == "turn.finished");
        if let (Some(cancel), true) = (cancel, finished) {
            settled = Some((cancel, events));
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (cancel, events) = settled.expect("the turn kept running to its end");
    assert_eq!(cancel["data"]["id"], asked.id.as_str());
    assert_eq!(cancel["data"]["reason"], "no_responder");
    let finished = events
        .iter()
        .find(|e| e["event"] == "turn.finished")
        .unwrap();
    assert_eq!(finished["data"]["final_response"], "denied");
}

#[tokio::test]
async fn a_session_left_without_clients_is_closed_after_the_idle_limit() {
    let mut options = options();
    options.idle_session_close = Duration::from_millis(300);
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    let (client, _inbox) = daemon.client("default", false).await;
    let session = create(&client).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        daemon.host.closed.lock().unwrap().is_empty(),
        "attached sessions stay open"
    );
    client
        .call("session.detach", json!({"session_id": session}))
        .await
        .unwrap();
    let mut closed = false;
    for _ in 0..100 {
        if daemon.host.closed.lock().unwrap().contains(&session) {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "closed once nobody used it for the idle limit");
    assert_eq!(
        daemon
            .host
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|s| **s == session)
            .count(),
        1,
        "closed once"
    );
}
