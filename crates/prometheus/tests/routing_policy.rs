//! Session-start routing through the binary (spec: model-providers, Cost
//! and latency aware routing policy): the policy's choice is recorded in
//! `provider.route_selected`, written to the session row and kept for
//! every turn of the session.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::Store;
use pmt_store::provider_state::CallSample;
use serde_json::Value;

use common::pty::{exit, prompt, spawn};
use common::{append_config, mock_home, ok, run};
use expectrl::Expect;

const ROUTING: &str = "\n[routing]\npolicy = \"fastest\"\nhealth_window = 3\n\
                       [[routing.candidates]]\nprovider = \"mock\"\nmodel = \"b\"\n\
                       [[routing.candidates]]\nprovider = \"mock\"\nmodel = \"c\"\n";

fn store(home: &Path) -> Arc<Store> {
    let home = Home::open(home).unwrap();
    let dir = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

/// Three successful calls of `mock/<model>` with time to first token `ms`.
fn measured(store: &Store, model: &str, ms: u64) {
    for _ in 0..3 {
        store
            .record_provider_call(&CallSample {
                provider: "mock".to_owned(),
                model: model.to_owned(),
                credential: "none".to_owned(),
                ts: Timestamp::now(),
                class: None,
                ttft_ms: Some(ms),
                output_tps: None,
            })
            .unwrap();
    }
}

/// The events of every finished session's trajectory, by session.
fn events(home: &Path) -> Vec<Value> {
    let dir = home.join("profiles/default/trajectories");
    let mut events = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let bytes = std::fs::read(entry.path()).unwrap();
        let text = if entry.path().extension().is_some_and(|e| e == "zst") {
            zstd::decode_all(bytes.as_slice()).unwrap()
        } else {
            bytes
        };
        events.extend(
            String::from_utf8(text)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
        );
    }
    events
}

fn of_type<'a>(events: &'a [Value], session: &str, kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["session_id"] == session && e["type"] == kind)
        .map(|e| &e["payload"])
        .collect()
}

/// Scenario: Route is stable within a session.
#[test]
fn a_route_chosen_at_start_stays_for_every_turn() {
    let home = mock_home();
    append_config(home.path(), ROUTING);
    let db = store(home.path());
    measured(&db, "b", 400);
    measured(&db, "c", 3_000);
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("one").unwrap();
    p.expect("mock: one").unwrap();
    prompt(&mut p);
    // C becomes faster than B during the session.
    measured(&db, "c", 1);
    p.send_line("two").unwrap();
    p.expect("mock: two").unwrap();
    prompt(&mut p);
    exit(p);
    let first = common::newest_session(home.path());
    // A session starting now picks C.
    ok(&run(home.path(), &["chat", "-z", "three"]));
    let second = common::newest_session(home.path());
    assert_ne!(first, second);

    let events = events(home.path());
    let chosen = of_type(&events, &first, "provider.route_selected");
    assert_eq!(chosen.len(), 1, "{chosen:?}");
    assert_eq!(chosen[0]["policy"], "fastest");
    assert_eq!(chosen[0]["selected"], "mock/b");
    assert_eq!(chosen[0]["rejected"][0]["route"], "mock/c");
    assert_eq!(chosen[0]["rejected"][0]["reason"], "latency");
    let models: Vec<&Value> = of_type(&events, &first, "model.call")
        .iter()
        .map(|call| &call["model"])
        .collect();
    assert_eq!(models, ["b", "b"]);
    let listed: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "show", &first],
    )))
    .unwrap();
    assert_eq!(listed["session"]["model"], "b", "{listed}");

    let later = of_type(&events, &second, "provider.route_selected");
    assert_eq!(later[0]["selected"], "mock/c");
    let models: Vec<&Value> = of_type(&events, &second, "model.call")
        .iter()
        .map(|call| &call["model"])
        .collect();
    assert_eq!(models, ["c"]);
}

#[test]
fn the_default_pinned_policy_records_no_choice() {
    let home = mock_home();
    ok(&run(home.path(), &["chat", "-z", "hello"]));
    let session = common::newest_session(home.path());
    let events = events(home.path());
    assert!(of_type(&events, &session, "provider.route_selected").is_empty());
    let models: Vec<&Value> = of_type(&events, &session, "model.call")
        .iter()
        .map(|call| &call["model"])
        .collect();
    assert_eq!(models, ["mock"]);
}
