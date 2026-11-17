//! A local model's window and resources on the daemon's managed server
//! (spec: local-models, Context window and memory placement; Local
//! resource telemetry), with the `pmt-llama-stub` engine: the status
//! bar's context gauge uses the window the server runs with, the session
//! status carries GPU and RAM use, and the session's growth port
//! relaunches the model at a larger window (or says why it cannot); a
//! daemon turn that reaches the compression threshold grows the window
//! instead of compressing and tells the session's clients.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use pmt_agent::turn::growth::{Growth, WindowGrowth as _};
use pmt_agent::turn::model::RouteInfo;
use serde_json::{Value, json};

use common::local::{FIXTURES_VAR, GIB, fixtures, local_home};
use common::{daemon, ok, run};

fn rpc(home: &Path, method: &str, params: &Value) -> Value {
    serde_json::from_str(&ok(&run(
        home,
        &["rpc", "call", method, &params.to_string()],
    )))
    .unwrap()
}

/// Submits `text` and waits for its answer.
fn turn(home: &Path, session: &str, text: &str) {
    let before = transcript_len(home, session);
    rpc(
        home,
        "prompt.submit",
        &json!({"session_id": session, "text": text}),
    );
    let deadline = Instant::now() + Duration::from_mins(1);
    while transcript_len(home, session) < before + 2 {
        assert!(Instant::now() < deadline, "the turn never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn transcript_len(home: &Path, session: &str) -> usize {
    rpc(home, "session.transcript", &json!({"session_id": session}))["messages"]
        .as_array()
        .map_or(0, Vec::len)
}

fn usage(home: &Path, session: &str) -> Value {
    rpc(
        home,
        "session.context_usage",
        &json!({"session_id": session}),
    )
}

fn stub_log(home: &Path) -> String {
    std::fs::read_to_string(home.join("run/pmt-llama-stub.log")).unwrap_or_default()
}

/// Scenarios: Gauge shows real window; Window grows before compression
/// (the relaunch); Growth impossible triggers compression (the answer).
#[test]
fn the_gauge_uses_the_served_window_and_the_managed_model_grows() {
    let home = local_home();
    let h = home.path();
    let fixtures = fixtures(h);
    let _daemon = daemon::start_env(h, &[(FIXTURES_VAR, fixtures.to_str().unwrap())]);
    let session = rpc(h, "session.create", &json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    turn(h, &session, "hello");
    let first = usage(h, &session);
    // The listing says 262144 (the training window); the server runs 65536.
    assert_eq!(first["context_window"], 65_536, "{first}");
    let resources = &first["local_resources"];
    assert_eq!(resources["gpu_memory_total"], 24 * GIB, "{first}");
    assert_eq!(resources["gpu_utilization_percent"], 37.0);
    assert_eq!(resources["ram_used"], 16 * GIB);

    // The turn loop asks its growth port before compressing (proved with
    // a fake port in pmt-agent's growth tests); here the port itself
    // relaunches the managed server's model at a larger window.
    let catalog = std::sync::Arc::new(pmt_providers::metadata::Catalog::new(
        &pmt_config::Config::default(),
        None,
    ));
    let growth =
        prometheus::local_cmd::growth::LocalGrowth::new(h, std::sync::Arc::clone(&catalog));
    let route = RouteInfo {
        provider: "llamacpp".to_owned(),
        model: "qwen-x".to_owned(),
        price: None,
        context_window: Some(65_536),
        max_output_tokens: None,
        wire_format: pmt_config::schema::WireFormat::ChatCompletions,
        billing_source: "local".to_owned(),
        server_compaction: false,
        local: true,
        fast_tier: false,
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let grown = runtime.block_on(growth.grow(&session, &route, 70_000));
    assert_eq!(
        grown,
        Growth::Grown {
            from: 65_536,
            to: 131_072
        }
    );
    let record = std::fs::read_to_string(h.join("models/qwen-x/model.json")).unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["context"], 131_072);
    let log = stub_log(h);
    assert!(
        log.contains("reload") && log.contains("unload qwen-x"),
        "{log}"
    );
    // The window re-reported to the gauge is the one the server reloads.
    let (meta, _) = catalog.metadata_at("llamacpp", "qwen-x", None);
    assert_eq!(meta.context_window, 131_072);
    let next = rpc(h, "session.create", &json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    turn(h, &next, "again");
    assert!(
        stub_log(h).contains("load qwen-x c=131072"),
        "{}",
        stub_log(h)
    );
    let after = usage(h, &next);
    assert_eq!(after["context_window"], 131_072, "{after}\n{}", stub_log(h));

    // At the native maximum the window cannot grow: compression runs.
    let route = RouteInfo {
        context_window: Some(262_144),
        ..route
    };
    let mut record = record;
    record["context"] = 262_144.into();
    std::fs::write(h.join("models/qwen-x/model.json"), record.to_string()).unwrap();
    let blocked = runtime.block_on(growth.grow(&session, &route, 300_000));
    assert_eq!(blocked, Growth::Impossible("native_max".to_owned()));
}

/// Scenario: Window grows before compression, through a daemon turn: a
/// prompt that takes the conversation past the compression trigger relaunches the model at 131072, the session's
/// clients receive `local_model.context_grown`, the request is served at
/// the grown window and the conversation is not compressed.
#[test]
fn a_daemon_turn_past_the_threshold_grows_the_window_and_tells_the_clients() {
    let home = local_home();
    let h = home.path();
    let fixtures = fixtures(h);
    let _daemon = daemon::start_env(h, &[(FIXTURES_VAR, fixtures.to_str().unwrap())]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (session, events) = runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(h, "default").await;
        let created = client.call("session.create", json!({})).await.unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        // The first turn's reported usage anchors the measure.
        let mut events = submit(&client, &mut inbox, &session, "hello").await;
        // 60000 tokens by the estimate (4 bytes a token): past the
        // trigger (55705 tokens of the 65536 window at threshold 0.5,
        // after the window's effective-threshold adjustment), within the
        // window.
        let long = "word ".repeat(48_000);
        events.extend(submit(&client, &mut inbox, &session, &long).await);
        (session, events)
    });
    let grown: Vec<&Value> = events
        .iter()
        .filter(|(name, _)| name == "local_model.context_grown")
        .map(|(_, data)| data)
        .collect();
    assert_eq!(grown.len(), 1, "{events:?}");
    assert_eq!(
        grown[0],
        &json!({"session_id": session, "provider": "llamacpp", "model": "qwen-x",
            "from": 65_536, "to": 131_072})
    );
    let log = stub_log(h);
    assert!(log.contains("load qwen-x c=131072"), "{log}");
    // Not compressed: every message is still in the active transcript.
    assert_eq!(transcript_len(h, &session), 4);
    assert_eq!(usage(h, &session)["context_window"], 131_072);
}

/// Submits `text` on `session` and returns the events until its turn
/// finished (name and data).
async fn submit(
    client: &pmt_rpc::client::Client,
    inbox: &mut pmt_rpc::client::Inbox,
    session: &str,
    text: &str,
) -> Vec<(String, Value)> {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, inbox.events.recv()).await {
        let done = event.event == "turn.finished";
        events.push((event.event, event.data));
        if done {
            return events;
        }
    }
    panic!("the turn never finished: {events:?}")
}
