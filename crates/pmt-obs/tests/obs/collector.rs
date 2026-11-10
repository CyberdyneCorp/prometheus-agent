//! The OTLP export against a real OpenTelemetry Collector (spec:
//! observability, Opt-in OpenTelemetry export): `scripts/ci/otel-collector`
//! starts the collector container and runs this ignored test with its
//! endpoints; the collector writes what it decoded as OTLP JSON lines,
//! which must hold the exported spans and metrics of both protocols.

use std::time::{Duration, Instant};

use pmt_config::schema::OtelProtocol;
use pmt_obs::otel::{ExportSettings, Exporter, Identity, SessionTrace, install};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{SessionEnd, TurnEnd, TurnStart, Usage};
use pmt_obs::trajectory::writer::Emit;

fn export(endpoint: &str, protocol: OtelProtocol, session: &str) -> String {
    let exporter = Exporter::start(ExportSettings {
        endpoint: endpoint.to_owned(),
        protocol,
        headers: Vec::new(),
        capture_content: false,
        propagate_external: false,
        outbound: pmt_net::OutboundPolicy::default(),
        resource: vec![("service.name".to_owned(), "prometheus".to_owned())],
    })
    .unwrap();
    install(Some(exporter.clone()));
    let trace = SessionTrace::open(
        Identity {
            profile: "default".to_owned(),
            session_id: session.to_owned(),
            root_session_id: session.to_owned(),
            source: "cli".to_owned(),
        },
        None,
        None,
    );
    trace.observe(&Emit::now(Event::TurnStart(TurnStart { message_id: None })).in_turn("t", None));
    trace.observe(
        &Emit::now(Event::TurnEnd(TurnEnd {
            reason: "completed".to_owned(),
            iterations: 1,
            usage: Usage::default(),
            cache_hit_rate: None,
            context_tokens: None,
            compaction_trigger_tokens: None,
        }))
        .in_turn("t", None),
    );
    trace.observe(&Emit::now(Event::SessionEnd(SessionEnd {
        reason: "user_exit".to_owned(),
    })));
    exporter.flush_blocking(Duration::from_secs(15));
    let stats = exporter.stats();
    assert!(
        stats.exported >= 2 && stats.last_error.is_none(),
        "{protocol:?}: {stats:?}"
    );
    trace.trace_hex()
}

#[test]
#[ignore = "needs an OpenTelemetry Collector: scripts/ci/otel-collector"]
fn real_collector_accepts_http_protobuf_and_grpc_exports() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let output = var("PMT_TEST_OTEL_OUTPUT");
    let http = export(
        &var("PMT_TEST_OTEL_HTTP"),
        OtelProtocol::HttpProtobuf,
        "ses_otlp_http",
    );
    let grpc = export(
        &var("PMT_TEST_OTEL_GRPC"),
        OtelProtocol::Grpc,
        "ses_otlp_grpc",
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let written = std::fs::read_to_string(&output).unwrap_or_default();
        let done = [
            http.as_str(),
            grpc.as_str(),
            "\"turn\"",
            "prometheus.sessions.active",
        ]
        .iter()
        .all(|needle| written.contains(needle));
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the collector did not write every export: {written}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
