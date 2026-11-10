//! Opt-in OpenTelemetry export (spec: observability, Opt-in OpenTelemetry
//! export; Span model; Trace context propagation; Metrics catalog),
//! against a local fake OTLP/HTTP collector that decodes what it receives
//! with the same protobuf messages a real collector parses.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_config::schema::OtelProtocol;
use pmt_obs::metrics;
use pmt_obs::otel::proto::{ExportTraceServiceRequest, Span, string_attr};
use pmt_obs::otel::{ExportSettings, Exporter, Identity, SessionTrace, install};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{
    MessageText, SessionEnd, ToolCall, ToolResult, TurnEnd, TurnStart, Usage,
};
use pmt_obs::trajectory::writer::Emit;
use prost::Message as _;
use serde_json::json;

/// Request bodies a fake collector received, by path.
#[derive(Default)]
struct Received {
    bodies: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Received {
    fn spans(&self) -> Vec<Span> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/traces")
            .flat_map(|(_, body)| {
                ExportTraceServiceRequest::decode(body.as_slice())
                    .unwrap()
                    .resource_spans
            })
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect()
    }

    fn raw(&self) -> Vec<u8> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, b)| b.clone())
            .collect()
    }
}

/// A fake OTLP/HTTP collector on a loopback port: answers 200 to every
/// POST and keeps the bodies.
fn collector() -> (String, Arc<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Received::default());
    let keep = Arc::clone(&received);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let keep = Arc::clone(&keep);
            std::thread::spawn(move || serve(stream, &keep));
        }
    });
    (url, received)
}

fn serve(stream: std::net::TcpStream, received: &Received) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        received.bodies.lock().unwrap().push((path, body));
        let _ = writer.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
    }
}

fn exporter(endpoint: &str, capture_content: bool) -> Exporter {
    let exporter = Exporter::start(ExportSettings {
        endpoint: endpoint.to_owned(),
        protocol: OtelProtocol::HttpProtobuf,
        headers: vec![("x-api-key".to_owned(), "k".to_owned())],
        capture_content,
        propagate_external: false,
        outbound: pmt_net::OutboundPolicy::default(),
        resource: vec![("service.name".to_owned(), "prometheus".to_owned())],
    })
    .unwrap();
    install(Some(exporter.clone()));
    exporter
}

fn identity(session: &str, root: &str, source: &str) -> Identity {
    Identity {
        profile: "default".to_owned(),
        session_id: session.to_owned(),
        root_session_id: root.to_owned(),
        source: source.to_owned(),
    }
}

fn at(event: Event, turn: &str) -> Emit {
    Emit::now(event).in_turn(turn, None)
}

/// One turn of `trace` that reads `confidential plan` from a tool.
fn turn(trace: &SessionTrace, prompt: &str) {
    trace.observe(&at(
        Event::TurnStart(TurnStart { message_id: None }),
        "turn_1",
    ));
    trace.observe(&at(
        Event::UserMessage(MessageText {
            message_id: None,
            text: prompt.to_owned(),
        }),
        "turn_1",
    ));
    trace.observe(&at(
        Event::ToolCall(ToolCall {
            call_id: "call_1".to_owned(),
            tool: "read_file".to_owned(),
            arguments: json!({"path": "plan.md", "note": prompt}),
        }),
        "turn_1",
    ));
    trace.observe(&at(
        Event::ToolResult(ToolResult {
            call_id: "call_1".to_owned(),
            tool: "read_file".to_owned(),
            ok: true,
            duration_ms: Some(3),
            output: Some(format!("the file says: {prompt}")),
            error: None,
        }),
        "turn_1",
    ));
    trace.observe(&at(
        Event::TurnEnd(TurnEnd {
            reason: "completed".to_owned(),
            iterations: 1,
            usage: Usage::default(),
            cache_hit_rate: None,
            context_tokens: None,
            compaction_trigger_tokens: None,
        }),
        "turn_1",
    ));
    trace.observe(&Emit::now(Event::SessionEnd(SessionEnd {
        reason: "user_exit".to_owned(),
    })));
}

#[test]
fn collector_down_adds_no_latency_and_counts_dropped_items() {
    // A port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let exporter = exporter(&format!("http://127.0.0.1:{port}"), false);
    let before = metrics::global()
        .snapshot()
        .value(metrics::OTEL_DROPPED, &[]);
    let trace = SessionTrace::open(identity("ses_down", "ses_down", "cli"), None, None, None);
    let started = Instant::now();
    turn(&trace, "hello");
    let spent = started.elapsed();
    assert!(
        spent < Duration::from_millis(5),
        "recording a turn took {spent:?}"
    );
    exporter.flush_blocking(Duration::from_secs(10));
    let stats = exporter.stats();
    assert!(stats.dropped >= 3, "{stats:?}");
    assert!(stats.last_error.is_some());
    let after = metrics::global()
        .snapshot()
        .value(metrics::OTEL_DROPPED, &[]);
    assert!(after > before, "prometheus.otel.dropped did not increase");
}

#[test]
fn a_full_queue_drops_the_oldest_and_counts_it() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let exporter = exporter(&format!("http://127.0.0.1:{port}"), false);
    let total = pmt_obs::otel::export::QUEUE_CAPACITY + 10;
    for _ in 0..total {
        exporter.push_span(Span::default());
    }
    assert!(exporter.stats().queued <= pmt_obs::otel::export::QUEUE_CAPACITY as u64);
    exporter.flush_blocking(Duration::from_secs(10));
    // Every item was either dropped (overflow or failed export) or still waits.
    let stats = exporter.stats();
    assert_eq!(stats.dropped + stats.queued, total as u64, "{stats:?}");
    assert_eq!(stats.exported, 0);
}

#[test]
fn content_is_not_exported_by_default() {
    let (url, received) = collector();
    let exporter = exporter(&url, false);
    let trace = SessionTrace::open(identity("ses_c", "ses_c", "cli"), None, None, None);
    turn(&trace, "confidential plan");
    exporter.flush_blocking(Duration::from_secs(10));
    let spans = received.spans();
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    for expected in ["session", "turn", "tool.call"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let raw = received.raw();
    assert!(
        !raw.windows(17).any(|w| w == b"confidential plan"),
        "an exported span or metric carries the prompt"
    );
}

#[test]
fn capture_content_exports_prompt_arguments_and_output() {
    let (url, received) = collector();
    let exporter = exporter(&url, true);
    let trace = SessionTrace::open(identity("ses_k", "ses_k", "cli"), None, None, None);
    turn(&trace, "confidential plan");
    exporter.flush_blocking(Duration::from_secs(10));
    let spans = received.spans();
    let tool = spans.iter().find(|s| s.name == "tool.call").unwrap();
    assert!(
        string_attr(&tool.attributes, "prometheus.tool.arguments")
            .unwrap()
            .contains("confidential plan")
    );
    assert!(
        string_attr(&tool.attributes, "prometheus.tool.output")
            .unwrap()
            .contains("confidential plan")
    );
    let turn = spans.iter().find(|s| s.name == "turn").unwrap();
    assert_eq!(
        string_attr(&turn.attributes, "prometheus.input"),
        Some("confidential plan")
    );
}

/// Regression (spec: security-boundaries, Known-secret redaction
/// everywhere): captured content is redacted with the profile's redactor
/// before it leaves the host, as the trajectory is.
#[test]
fn captured_content_is_redacted_before_export() {
    let (url, received) = collector();
    let exporter = exporter(&url, true);
    let secret = pmt_core::secret::SecretString::new("hunter2-very-secret-value");
    let loader: pmt_obs::otel::RedactorLoader = Box::new(move || {
        Some(pmt_core::secret::Redactor::named([(
            "DB_PASSWORD",
            &secret,
        )]))
    });
    let trace = SessionTrace::open(identity("ses_r", "ses_r", "cli"), None, None, Some(loader));
    turn(&trace, "connect with hunter2-very-secret-value");
    exporter.flush_blocking(Duration::from_secs(10));
    let raw = received.raw();
    assert!(
        !raw.windows(25).any(|w| w == b"hunter2-very-secret-value"),
        "the secret left the host"
    );
    let spans = received.spans();
    let turn = spans.iter().find(|s| s.name == "turn").unwrap();
    assert_eq!(
        string_attr(&turn.attributes, "prometheus.input"),
        Some("connect with [REDACTED:DB_PASSWORD]")
    );
    let tool = spans.iter().find(|s| s.name == "tool.call").unwrap();
    assert!(
        string_attr(&tool.attributes, "prometheus.tool.output")
            .unwrap()
            .contains("[REDACTED:DB_PASSWORD]")
    );
}

#[test]
fn subagent_spans_join_the_parent_trace() {
    let (url, received) = collector();
    let exporter = exporter(&url, false);
    let parent = SessionTrace::open(
        identity("ses_parent", "ses_parent", "cli"),
        None,
        None,
        None,
    );
    parent.observe(&at(
        Event::TurnStart(TurnStart { message_id: None }),
        "turn_p",
    ));
    parent.observe(&at(
        Event::ToolCall(ToolCall {
            call_id: "call_delegate".to_owned(),
            tool: "delegate_task".to_owned(),
            arguments: json!({"tasks": ["a", "b"]}),
        }),
        "turn_p",
    ));
    for child in ["ses_child_a", "ses_child_b"] {
        let trace = SessionTrace::open(
            identity(child, "ses_parent", "subagent"),
            Some("ses_parent"),
            None,
            None,
        );
        trace.observe(&Emit::now(Event::SessionEnd(SessionEnd {
            reason: "completed".to_owned(),
        })));
    }
    parent.observe(&at(
        Event::ToolResult(ToolResult {
            call_id: "call_delegate".to_owned(),
            tool: "delegate_task".to_owned(),
            ok: true,
            duration_ms: Some(10),
            output: None,
            error: None,
        }),
        "turn_p",
    ));
    exporter.flush_blocking(Duration::from_secs(10));
    let spans = received.spans();
    let tool = spans.iter().find(|s| s.name == "tool.call").unwrap();
    let children: Vec<&Span> = spans
        .iter()
        .filter(|s| {
            s.name == "session"
                && string_attr(&s.attributes, "prometheus.source") == Some("subagent")
        })
        .collect();
    assert_eq!(children.len(), 2);
    for child in children {
        assert_eq!(child.trace_id, tool.trace_id);
        assert_eq!(child.parent_span_id, tool.span_id);
    }
    assert_eq!(
        parent.trace_hex(),
        pmt_obs::otel::context::trace_hex_for_root("ses_parent")
    );
}

#[test]
fn tool_failures_count_by_tool_outcome_and_kind() {
    install(None);
    let trace = SessionTrace::open(identity("ses_m", "ses_m", "cli"), None, None, None);
    trace.observe(&at(
        Event::ToolResult(ToolResult {
            call_id: "c".to_owned(),
            tool: "terminal".to_owned(),
            ok: false,
            duration_ms: Some(1),
            output: None,
            error: Some(pmt_obs::trajectory::payload::Failure {
                kind: "timeout".to_owned(),
                code: None,
                message: "slow".to_owned(),
            }),
        }),
        "t",
    ));
    let snapshot = metrics::global().snapshot();
    assert!(
        (snapshot.value(
            metrics::TOOL_CALLS,
            &[("tool", "terminal"), ("outcome", "error")]
        ) - 1.0)
            .abs()
            < f64::EPSILON
    );
    assert!((snapshot.value(metrics::ERRORS, &[("kind", "timeout")]) - 1.0).abs() < f64::EPSILON);
    assert!((snapshot.value(metrics::SESSIONS_ACTIVE, &[]) - 1.0).abs() < f64::EPSILON);
    drop(trace);
    assert!(
        metrics::global()
            .snapshot()
            .value(metrics::SESSIONS_ACTIVE, &[])
            .abs()
            < f64::EPSILON
    );
}
