//! The `NeMo` Relay host (spec: observability, `NeMo` Relay and Langfuse
//! integrations): a session's lifecycle reaches Relay, hosted in the
//! python bridge in a managed environment, with payloads only under
//! `capture_content`. A fake `nemo-relay` from a local index records what
//! the host does with it. Needs uv on PATH (see `pmt_pybridge::testkit`).

mod common;

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;

use self::common::{append_config, command, mock_home, ok};

/// A `nemo_relay` that logs its calls next to the `plugins.toml` it was
/// initialized with.
const FAKE: &[(&str, &str)] = &[
    (
        "nemo_relay/__init__.py",
        r#"
from . import plugin, scope, subscribers

class ScopeType:
    Agent = "Agent"
    Function = "Function"
    Tool = "Tool"
    Llm = "Llm"
"#,
    ),
    (
        "nemo_relay/plugin.py",
        r#"
import json, os

LOG = None

def record(entry):
    with open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")

async def initialize(config, additional_plugins_toml=None):
    global LOG
    LOG = os.path.join(os.path.dirname(additional_plugins_toml), "relay-log.jsonl")
    record({"call": "initialize", "plugins_toml": additional_plugins_toml})
    return object()
"#,
    ),
    (
        "nemo_relay/scope.py",
        r#"
import itertools
from .plugin import record

IDS = itertools.count(1)

def push(name, scope_type, *, handle=None, metadata=None, input=None):
    sid = next(IDS)
    entry = {"call": "push", "id": sid, "name": name, "type": scope_type, "parent": handle}
    if input is not None:
        entry["input"] = input
    record(entry)
    return sid

def pop(handle, *, output=None, metadata=None):
    entry = {"call": "pop", "id": handle}
    if output is not None:
        entry["output"] = output
    record(entry)

def event(name, *, handle=None, data=None):
    record({"call": "event", "name": name, "parent": handle, "data": data})
"#,
    ),
    (
        "nemo_relay/subscribers.py",
        "from .plugin import record\n\ndef flush():\n    record({\"call\": \"flush\"})\n",
    ),
];

/// A home with the fake Relay indexed and enabled; `capture` sets
/// `observability.otel.capture_content`.
fn relay_home(capture: bool) -> (tempfile::TempDir, pmt_pybridge::testkit::Served) {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let index = home.path().join("index");
    pmt_pybridge::testkit::index(&index, &[("nemo-relay", "0.11.1", FAKE)]);
    let served = pmt_pybridge::testkit::serve(&index);
    let plugins = home.path().join("relay").join("plugins.toml");
    std::fs::create_dir_all(plugins.parent().unwrap()).unwrap();
    std::fs::write(&plugins, "version = 1\n").unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[observability.otel]\ncapture_content = {capture}\n\n[observability.nemo_relay]\nenabled = true\nplugins_toml = {:?}\n",
            plugins.to_str().unwrap()
        ),
    );
    (home, served)
}

fn script(home: &Path) -> std::path::PathBuf {
    let path = home.join("script.toml");
    std::fs::write(
        &path,
        r#"
[[rule]]
user = "look"
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "notes.txt" }

[[rule]]
tool_result = "remember"
text = "the notes say remember"
"#,
    )
    .unwrap();
    path
}

/// Runs one turn and returns what Relay was told.
fn one_turn(home: &Path, url: &str) -> Vec<Value> {
    std::fs::write(home.join("notes.txt"), "remember").unwrap();
    let script = script(home);
    ok(&command(home)
        .current_dir(home)
        .args(["-z", "look at the notes", "--mock-script"])
        .arg(&script)
        .env("UV_DEFAULT_INDEX", url)
        .stdin(Stdio::null())
        .output()
        .unwrap());
    let log = std::fs::read_to_string(home.join("relay").join("relay-log.jsonl"))
        .expect("Relay was initialized");
    log.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn pushed<'a>(log: &'a [Value], kind: &str) -> Vec<&'a Value> {
    log.iter()
        .filter(|e| e["call"] == "push" && e["type"] == kind)
        .collect()
}

#[test]
fn a_session_reaches_relay_as_nested_scopes_without_payloads() {
    let (home, served) = relay_home(false);
    let log = one_turn(home.path(), &served.url);
    assert_eq!(log[0]["call"], "initialize");
    assert!(
        log[0]["plugins_toml"]
            .as_str()
            .unwrap()
            .ends_with("plugins.toml")
    );
    let session = pushed(&log, "Agent");
    let turn = pushed(&log, "Function");
    let tool = pushed(&log, "Tool");
    let llm = pushed(&log, "Llm");
    assert_eq!(session.len(), 1, "{log:?}");
    assert_eq!(turn.len(), 1, "{log:?}");
    assert_eq!(turn[0]["parent"], session[0]["id"]);
    assert_eq!(tool[0]["name"], "read_file");
    assert_eq!(tool[0]["parent"], turn[0]["id"]);
    assert!(llm.len() >= 2, "one per model call: {log:?}");
    assert!(llm.iter().all(|l| l["parent"] == turn[0]["id"]));
    // Every scope is closed, and nothing carries content.
    for scope in log.iter().filter(|e| e["call"] == "push") {
        assert!(
            log.iter()
                .any(|e| e["call"] == "pop" && e["id"] == scope["id"]),
            "{scope} never closed: {log:?}"
        );
    }
    assert!(
        log.iter()
            .all(|e| e.get("input").is_none() && e.get("output").is_none()),
        "{log:?}"
    );
    assert!(log.iter().all(|e| e["call"] != "event"), "{log:?}");
    assert_eq!(log.last().unwrap()["call"], "flush");
}

#[test]
fn captured_content_carries_the_tool_payloads_and_messages() {
    let (home, served) = relay_home(true);
    let log = one_turn(home.path(), &served.url);
    let tool = pushed(&log, "Tool");
    assert!(
        tool[0]["input"].as_str().unwrap().contains("notes.txt"),
        "{log:?}"
    );
    let closed = log
        .iter()
        .find(|e| e["call"] == "pop" && e["id"] == tool[0]["id"])
        .unwrap();
    assert!(
        closed["output"].as_str().unwrap().contains("remember"),
        "{log:?}"
    );
    let said: Vec<&str> = log
        .iter()
        .filter(|e| e["call"] == "event")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        said.contains(&"user.message") && said.contains(&"assistant.message"),
        "{log:?}"
    );
}
