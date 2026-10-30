//! No outbound telemetry by default (spec: observability, No outbound
//! telemetry by default; security-boundaries, Telemetry is opt-in and
//! HTTPS-only).
//!
//! Every outbound HTTP client is configured by `pmt_net` (which counts
//! them and logs each at DEBUG). With a fresh profile, a session made of
//! every M1 piece that exists (store, transcript, trajectory, usage ledger
//! with pricing, budget checks, logs, telemetry resolution) and the
//! commands that read it configure none. Its own test binary keeps the
//! process-wide counter free of other tests' clients.

mod common;

use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_obs::budget::{Budgets, Place, Request};
use pmt_obs::telemetry::Telemetry;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{MessageText, SessionStart};
use pmt_obs::trajectory::writer::{Emit, open_for_session};
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_store::{EndReason, NewMessage, NewSession, Role, Source, Store, Tokens};

use common::{command, ok, temp_home};

fn local_session(home: &Path) -> String {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    assert_eq!(
        Telemetry::resolve(&scope.config().telemetry),
        Telemetry::Off
    );
    assert!(
        Telemetry::resolve(&scope.config().telemetry)
            .client(&pmt_net::OutboundPolicy::default())
            .unwrap()
            .is_none()
    );

    let store = Arc::new(Store::open(&scope).unwrap());
    let id = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let writer = open_for_session(&scope, &id.to_string(), &id.to_string())
        .unwrap()
        .unwrap();
    writer
        .blocking_record(Emit::now(Event::SessionStart(SessionStart {
            source: "cli".to_owned(),
            title: None,
            model: Some("mock/scripted".to_owned()),
            parent_session_id: None,
        })))
        .unwrap();
    store
        .append_message(id, &NewMessage::new(Role::User, "hello"))
        .unwrap();
    writer
        .blocking_record(Emit::now(Event::UserMessage(MessageText {
            message_id: None,
            text: "hello".to_owned(),
        })))
        .unwrap();

    let budgets = Budgets::new(pmt_obs::time::profile_zone(&scope.config().cron).unwrap());
    let place = Place::of(&store, id, Some(Timestamp::now())).unwrap();
    budgets
        .check(
            &store,
            &scope.config().budgets,
            &Request {
                place: &place,
                unattended: false,
                model: "scripted",
                price: None,
                input_tokens: 12,
                max_output_tokens: 64,
            },
            Timestamp::now(),
        )
        .unwrap();
    let recorder = UsageRecorder::new(
        Arc::clone(&store),
        Arc::new(ConfigPrices::from_config(scope.config())),
    );
    recorder
        .record(Call {
            tokens: Tokens {
                input: 12,
                output: 5,
                ..Tokens::default()
            },
            ..Call::new(id, "main", "mock", "scripted")
        })
        .unwrap();
    store
        .append_message(id, &NewMessage::new(Role::Assistant, "hi"))
        .unwrap();
    tracing::info!(session_id = %id, "turn finished");
    store.end_session(id, EndReason::UserExit).unwrap();
    writer.finish().unwrap();
    id.to_string()
}

#[test]
fn a_local_session_and_its_commands_configure_no_network_client() {
    let home = temp_home();
    let session = local_session(home.path());
    assert_eq!(
        pmt_net::clients_built(),
        0,
        "the session configured a client"
    );

    // The commands that read the session, with DEBUG logging so a client
    // configured in the binary would leave its record.
    for args in [
        vec!["sessions", "list"],
        vec!["sessions", "stats", session.as_str()],
        vec!["trajectory", "show", session.as_str()],
        vec!["usage"],
        vec!["logs", "errors"],
        vec!["config", "show"],
    ] {
        ok(&command(home.path())
            .env("PMT_LOG", "debug")
            .args(&args)
            .output()
            .unwrap());
    }
    let logs = [
        home.path().join("logs/prometheus.log"),
        home.path().join("profiles/default/logs/prometheus.log"),
    ];
    // (pmt-net's own tests show the record a configured client leaves.)
    for log in logs.iter().filter(|log| log.exists()) {
        let text = std::fs::read_to_string(log).unwrap();
        assert!(
            !text.contains("outbound HTTP client configured"),
            "{} names a configured client:\n{text}",
            log.display()
        );
    }
}

#[test]
fn a_plaintext_telemetry_endpoint_is_refused_and_logged() {
    let home = temp_home();
    ok(&command(home.path())
        .args([
            "--set",
            "telemetry.enabled=true",
            "--set",
            "telemetry.endpoint=http://example.com/ingest",
            "usage",
        ])
        .output()
        .unwrap());
    let log =
        std::fs::read_to_string(home.path().join("profiles/default/logs/prometheus.log")).unwrap();
    let refusal = log
        .lines()
        .find(|line| line.contains("refusing to send telemetry to http://example.com/ingest"))
        .unwrap_or_else(|| panic!("no refusal logged:\n{log}"));
    assert!(refusal.contains("\"level\":\"warn\""), "{refusal}");
}
