//! The daemon's side of the realtime voice chat mode (spec: voice-mode,
//! Realtime full-duplex voice chat mode (upgrade)): the call is created
//! with the profile's key on the daemon, the desktop gets only the call id
//! and the SDP answer, and the voice layer's tokens are rows of their own
//! in the usage ledger.

#![cfg(unix)]

mod common;

use common::{ok, run, run_with_stdin, temp_home};
use pmt_rpc::contracts::{
    RealtimeUsage, VoiceRealtimeStart, VoiceRealtimeStartParams, VoiceRealtimeUsage,
    VoiceRealtimeUsageParams,
};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "sk-realtime-daemon-only";
const ANSWER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\n";

fn home_with(mode: &str, base_url: &str) -> common::TestHome {
    let home = temp_home();
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let config = home.path().join("profiles/default/config.toml");
    let text = std::fs::read_to_string(&config).unwrap()
        + &format!(
            "\n[voice]\nchat_mode = \"{mode}\"\n\n[voice.realtime]\nbase_url = \"{base_url}\"\n\
             voice = \"cedar\"\ninstructions = \"Answer in English.\"\n\n\
             [models.\"openai/gpt-realtime\"]\nprice = {{ input = 4.0, output = 16.0 }}\n"
        );
    std::fs::write(&config, text).unwrap();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        &format!("{KEY}\n"),
    ));
    home
}

async fn provider() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/realtime/calls"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("location", "/v1/realtime/calls/rtc_test123")
                .insert_header("content-type", "application/sdp")
                .set_body_string(ANSWER),
        )
        .mount(&server)
        .await;
    server
}

fn usage_rows(home: &std::path::Path) -> Vec<(String, String, i64, i64)> {
    priced_rows(home)
        .into_iter()
        .map(|(purpose, model, input, output, _, _)| (purpose, model, input, output))
        .collect()
}

type Row = (String, String, i64, i64, Option<f64>, String);

fn priced_rows(home: &std::path::Path) -> Vec<Row> {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.prepare(
        "SELECT purpose, model, input_tokens, output_tokens, cost_usd, cost_status \
         FROM usage_ledger WHERE purpose = 'voice.realtime'",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// The first `usage.updated` payload `inbox` receives.
async fn usage_updated(inbox: &mut pmt_rpc::client::Inbox) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let event = inbox.events.recv().await.expect("an event");
            if event.event == "usage.updated" {
                return event.data;
            }
        }
    })
    .await
    .expect("a usage.updated event")
}

/// Scenario: Key stays on the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_creates_the_realtime_call_and_hands_out_no_key() {
    let server = provider().await;
    let home = home_with("realtime", &format!("{}/v1", server.uri()));
    let _daemon = common::daemon::start(home.path(), &[]);
    let (client, mut inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    let created = client.call("session.create", json!({})).await.unwrap();
    let session_id = created["session_id"].as_str().unwrap().to_owned();
    let call = client
        .call_typed::<VoiceRealtimeStart>(&VoiceRealtimeStartParams {
            profile: "default".to_owned(),
            session_id: session_id.clone(),
            sdp: "v=0\r\no=- offer\r\n".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(call.call_id, "rtc_test123");
    assert_eq!(call.sdp, ANSWER);
    assert_eq!(call.tool, "prometheus_agent");
    let answer = serde_json::to_string(&call).unwrap();
    assert!(
        !answer.contains(KEY),
        "the key reached the client: {answer}"
    );

    // The provider got the offer and a session with one delegated tool.
    let requests: Vec<Request> = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&requests[0].body).into_owned();
    assert!(body.contains("o=- offer"), "{body}");
    assert!(body.contains("\"name\":\"prometheus_agent\""), "{body}");
    assert!(body.contains("\"voice\":\"cedar\""), "{body}");
    assert!(body.contains("Answer in English."), "{body}");

    // Each response is one row of the voice layer, recorded once.
    for _ in 0..2 {
        client
            .call_typed::<VoiceRealtimeUsage>(&VoiceRealtimeUsageParams {
                profile: "default".to_owned(),
                call_id: call.call_id.clone(),
                response_id: "resp_1".to_owned(),
                usage: RealtimeUsage {
                    input_tokens: 120,
                    output_tokens: 80,
                    cached_tokens: 20,
                },
            })
            .await
            .unwrap();
    }
    let rows = usage_rows(home.path());
    assert_eq!(
        rows,
        vec![(
            "voice.realtime".to_owned(),
            "gpt-realtime".to_owned(),
            100,
            80
        )]
    );
    // Priced from the profile's catalog (100 x $4/M + 80 x $16/M; no
    // cache-read rate is configured) and announced to the attached client.
    let (.., cost, status) = priced_rows(home.path()).remove(0);
    assert_eq!(status, "estimated");
    assert!(
        cost.is_some_and(|usd| (usd - 0.001_68).abs() < 1e-9),
        "{cost:?}"
    );
    let updated = usage_updated(&mut inbox).await;
    assert_eq!(updated["call"]["purpose"], "voice.realtime", "{updated}");
    // A call this daemon never created records nothing.
    let unknown = client
        .call_typed::<VoiceRealtimeUsage>(&VoiceRealtimeUsageParams {
            profile: "default".to_owned(),
            call_id: "rtc_other".to_owned(),
            response_id: "resp_2".to_owned(),
            usage: RealtimeUsage::default(),
        })
        .await;
    assert!(unknown.is_err());
    assert_eq!(usage_rows(home.path()).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chained_mode_refuses_a_realtime_call() {
    let server = provider().await;
    let home = home_with("chained", &format!("{}/v1", server.uri()));
    let _daemon = common::daemon::start(home.path(), &[]);
    let (client, _inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    let created = client.call("session.create", json!({})).await.unwrap();
    let refused = client
        .call_typed::<VoiceRealtimeStart>(&VoiceRealtimeStartParams {
            profile: "default".to_owned(),
            session_id: created["session_id"].as_str().unwrap().to_owned(),
            sdp: "v=0\r\n".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("realtime"), "{refused}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
