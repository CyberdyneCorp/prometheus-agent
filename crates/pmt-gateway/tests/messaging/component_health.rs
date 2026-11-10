//! Adapters in the component health model (spec: observability, Component
//! health model): failures in a row make an adapter `degraded` with the
//! last error as the reason, and attached clients hear `health.changed`.

use std::time::Duration;

use pmt_gateway::messaging::{AdapterError, AdapterState};
use pmt_obs::health::{self, State};

use crate::support::{Harness, Setup, open, said};

fn telegram_health() -> Option<(State, String)> {
    health::global()
        .components(Some("default"))
        .into_iter()
        .find(|c| c.name == "adapter:telegram")
        .map(|c| (c.state, c.reason))
}

/// Scenario: Adapter loses its connection.
#[tokio::test]
async fn three_failed_polls_in_a_row_degrade_the_adapter_and_announce_it() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    assert_eq!(telegram_health().map(|(s, _)| s), Some(State::Ok));
    let session = h.sessions()[0].to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let client = pmt_gateway::hub::Peer {
        tx,
        minor: h.server.protocol().minor,
        advertised: std::sync::Arc::default(),
        server_requests: false,
        client_id: "tui-902".into(),
        role: pmt_rpc::contracts::AttachRole::Observer,
    };
    h.server.hub().attach(902_000, &session, client, None);
    let link = h.adapter("telegram").link.lock().unwrap().clone().unwrap();
    for attempt in 1..=3 {
        link.report(
            AdapterState::Retrying,
            Some(AdapterError {
                retryable: true,
                code: "telegram_unreachable".into(),
                message: format!("long poll failed (attempt {attempt})"),
            }),
        );
        if attempt < 3 {
            assert_eq!(
                telegram_health().map(|(s, _)| s),
                Some(State::Ok),
                "after {attempt}"
            );
        }
    }
    let (state, reason) = telegram_health().unwrap();
    assert_eq!(state, State::Degraded);
    assert!(reason.contains("long poll failed (attempt 3)"), "{reason}");
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = rx
                .recv()
                .await
                .expect("a frame")
                .into_text()
                .unwrap()
                .to_string();
            if text.contains("\"health.changed\"") {
                return text;
            }
        }
    })
    .await
    .expect("a health.changed event");
    assert!(
        frame.contains("\"component\":\"adapter:telegram\""),
        "{frame}"
    );
    assert!(frame.contains("\"state\":\"degraded\""), "{frame}");
}
