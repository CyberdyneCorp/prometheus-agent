//! Opt-in OpenTelemetry export from real sessions on the mock provider to
//! a local fake OTLP/HTTP collector (spec: observability, Opt-in
//! OpenTelemetry export; Span model; Trace context propagation).

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};

use common::{append_config, command, mock_home, ok, run, stderr};
use pmt_obs::otel::proto::{ExportTraceServiceRequest, Span, string_attr};
use prost::Message as _;
use serde_json::Value;

type Bodies = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A fake collector: 200 to every POST, bodies kept by path.
fn collector() -> (String, Bodies) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies: Bodies = Arc::default();
    let keep = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let keep = Arc::clone(&keep);
            std::thread::spawn(move || serve(stream, &keep));
        }
    });
    (url, bodies)
}

/// Answers every request of one connection with 200, keeping its body.
fn serve(stream: std::net::TcpStream, keep: &Bodies) {
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
        let body = common::read_http_request(&mut reader);
        keep.lock().unwrap().push((path, body));
        if writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .is_err()
        {
            return;
        }
    }
}

fn spans(bodies: &Bodies) -> Vec<Span> {
    bodies
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

fn exporting_home(url: &str) -> tempfile::TempDir {
    let home = mock_home();
    append_config(
        home.path(),
        &format!("\n[observability.otel]\nenabled = true\nendpoint = \"{url}\"\n"),
    );
    home
}

fn oneshot(home: &Path, prompt: &str, script: Option<&str>) -> Output {
    let mut cmd = command(home);
    cmd.current_dir(home)
        .env("PMT_LOG", "debug")
        .args(["--json", "-z", prompt]);
    if let Some(script) = script {
        let path = home.join("script.toml");
        std::fs::write(&path, script).unwrap();
        cmd.arg("--mock-script").arg(&path);
    }
    cmd.stdin(std::process::Stdio::null()).output().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    pmt_obs::otel::context::hex(bytes)
}

fn sessions(home: &Path) -> Vec<Value> {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    listed["sessions"].as_array().unwrap().clone()
}

#[test]
fn a_turn_exports_its_span_tree_under_the_session_trace_without_content() {
    let (url, bodies) = collector();
    let home = exporting_home(&url);
    let out = oneshot(home.path(), "confidential plan", None);
    assert!(out.status.success(), "{}", stderr(&out));
    let spans = spans(&bodies);
    let find = |name: &str| {
        spans.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "no {name} span in {:?}",
                spans.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
    };
    let (session, turn, call) = (find("session"), find("turn"), find("model.call"));
    assert_eq!(turn.parent_span_id, session.span_id);
    assert_eq!(call.parent_span_id, turn.span_id);
    assert_eq!(string_attr(&call.attributes, "gen_ai.system"), Some("mock"));
    assert!(string_attr(&call.attributes, "gen_ai.request.model").is_some());
    assert!(
        call.attributes
            .iter()
            .any(|a| a.key == "gen_ai.usage.input_tokens")
    );
    assert_eq!(
        string_attr(&call.attributes, "prometheus.purpose"),
        Some("main")
    );
    // Scenario: Content not exported by default.
    let raw: Vec<u8> = bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(_, b)| b.clone())
        .collect();
    assert!(!raw.windows(17).any(|w| w == b"confidential plan"));
    // The session's trace id is the one in its trajectory and log records.
    let trace = hex(&session.trace_id);
    let id = sessions(home.path())[0]["id"].as_str().unwrap().to_owned();
    let lines = ok(&run(
        home.path(),
        &["trajectory", "show", &id, "--format", "jsonl"],
    ));
    let start: Value = lines
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["type"] == "session.start")
        .unwrap();
    assert_eq!(start["payload"]["trace_id"], trace.as_str());
    let log =
        std::fs::read_to_string(home.path().join("profiles/default/logs/prometheus.log")).unwrap();
    let traced: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["session_id"] == id.as_str())
        .collect();
    assert!(!traced.is_empty(), "no log record of the session: {log}");
    assert!(
        traced.iter().all(|r| r["trace_id"] == trace.as_str()),
        "{traced:?}"
    );
}

/// Scenario: Subagent spans join the parent trace.
#[test]
fn subagent_session_spans_hang_under_the_delegating_tool_call() {
    let (url, bodies) = collector();
    let home = exporting_home(&url);
    let script = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "First errand" }, { goal = "Second errand" }] }

[[rule]]
user = "First errand"
text = "first done"

[[rule]]
user = "Second errand"
text = "second done"

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent saw the results"
"#;
    let out = oneshot(home.path(), "please delegate", Some(script));
    assert!(out.status.success(), "{}", stderr(&out));
    let spans = spans(&bodies);
    let tool = spans
        .iter()
        .find(|s| {
            s.name == "tool.call"
                && string_attr(&s.attributes, "prometheus.tool") == Some("delegate_task")
        })
        .expect("the delegate_task span");
    let children: Vec<&Span> = spans
        .iter()
        .filter(|s| {
            s.name == "session"
                && string_attr(&s.attributes, "prometheus.source") == Some("subagent")
        })
        .collect();
    assert_eq!(children.len(), 2, "{spans:?}");
    for child in children {
        assert_eq!(child.trace_id, tool.trace_id);
        assert_eq!(child.parent_span_id, tool.span_id);
    }
}

#[test]
fn export_without_an_endpoint_is_a_configuration_error() {
    let home = mock_home();
    append_config(home.path(), "\n[observability.otel]\nenabled = true\n");
    let out = oneshot(home.path(), "hello", None);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("observability.otel.endpoint"),
        "{}",
        stderr(&out)
    );
}
