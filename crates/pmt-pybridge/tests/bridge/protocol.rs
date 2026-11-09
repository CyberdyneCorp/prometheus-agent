//! The wire protocol and typed values (spec: python-bridge, Sidecar wire
//! protocol; Typed values across the wire; Supported `PluginContext` subset).

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use pmt_pybridge::log::Log;
use pmt_pybridge::sidecar::{CallError, Sidecar, Spawn};
use serde_json::{Value, json};

use crate::rig::{Recorder, quick, rig};

#[test]
fn a_tool_handler_runs_in_the_sidecar_and_its_return_is_the_result() {
    let rig = rig("echo", quick());
    let answer = rig
        .supervisor
        .tool(
            "echo",
            &json!({"text": "hi"}),
            &json!({"session_id": "s1", "call_id": "c1"}),
        )
        .unwrap();
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["args"], json!({"text": "hi"}));
    let registrations = rig.supervisor.registrations().unwrap();
    assert!(registrations.has_tool("whoami"));
    assert_eq!(registrations.hooks, ["post_tool_call"]);
    assert_eq!(
        registrations.sections[0].r#static.as_deref(),
        Some("Echo plugin rules.")
    );
    // register_automation_blueprint: data scheduled-jobs reads (spec:
    // scheduled-jobs, Automation blueprint catalog).
    assert_eq!(registrations.blueprints.len(), 1);
    assert_eq!(registrations.blueprints[0]["key"], "standup");
    assert_eq!(registrations.blueprints[0]["slots"][0]["default"], "09:15");
}

#[test]
fn a_provider_result_keeps_its_declared_dataclass_fields() {
    let rig = rig("echo", quick());
    let params = json!({"kind": "web_search", "provider": "echo-search", "method": "search", "args": {"query": "rust", "limit": 2}});
    let answer = rig
        .supervisor
        .call("provider.call", &params, Duration::from_secs(20))
        .unwrap();
    assert_eq!(
        answer,
        json!([
            {"title": "rust 0", "url": "https://example.com/0", "score": 0.5},
            {"title": "rust 1", "url": "https://example.com/1", "score": 0.5},
        ])
    );
}

#[test]
fn an_oversized_tool_result_fails_with_frame_too_large() {
    // Building and encoding a 20 MiB result can keep a slow runner's
    // sidecar from answering 200 ms pings for longer than the 3-miss
    // limit; this test is about the frame cap, not the heartbeat.
    let mut settings = quick();
    settings.ping_every = Duration::from_secs(5);
    let rig = rig("echo", settings);
    let err = rig
        .supervisor
        .tool("big", &json!({}), &json!({}))
        .unwrap_err();
    assert!(
        matches!(&err, CallError::Remote { code, .. } if code == "frame_too_large"),
        "{err:?}"
    );
    assert!(
        err.message().starts_with("frame_too_large"),
        "{}",
        err.message()
    );
    // The sidecar is still usable.
    rig.supervisor.tool("echo", &json!({}), &json!({})).unwrap();
}

#[test]
fn an_unknown_method_is_answered_with_32601() {
    let rig = rig("echo", quick());
    let sidecar = rig.supervisor.ensure().unwrap();
    let err = sidecar
        .call("bridge.bogus", &json!({}), Duration::from_secs(5))
        .unwrap_err();
    assert!(
        matches!(err, CallError::Remote { ref message, .. } if message.contains("unknown method")),
        "{err:?}"
    );
}

#[test]
fn a_narrow_hook_callback_gets_only_what_it_declares() {
    let rig = rig("echo", quick());
    let payload = json!({"tool": "write_file", "arguments": {"path": "a"}, "result": "ok", "session_id": "s", "call_id": "c"});
    let answer = rig
        .supervisor
        .call(
            "hook.invoke",
            &json!({"hook": "post_tool_call", "payload": payload}),
            Duration::from_secs(20),
        )
        .unwrap();
    assert_eq!(answer, json!({"results": [null]}));
    let written = std::fs::read_to_string(rig.data.join("hook.json")).unwrap();
    let seen: Value = serde_json::from_str(written.trim()).unwrap();
    assert_eq!(seen, json!({"tool_name": "write_file", "result": "ok"}));
}

#[test]
fn a_protocol_version_mismatch_stops_the_sidecar() {
    let script = r#"
import json, struct, sys
data = sys.stdin.buffer.read(4)
body = sys.stdin.buffer.read(struct.unpack(">I", data)[0])
msg = json.loads(body)
out = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocol_version": 2}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(out)) + out)
sys.stdout.buffer.flush()
import time; time.sleep(30)
"#;
    let home = tempfile::tempdir().unwrap();
    pmt_pybridge::testkit::seed(home.path());
    let python =
        pmt_pybridge::runtime::require(home.path(), pmt_pybridge::testkit::VERSION).unwrap();
    let mut command = Command::new(&python.exe);
    command.args(["-c", script]);
    let spawn = Spawn {
        command,
        plugin: "fake".to_owned(),
        log: Arc::new(Log::new(home.path().join("fake.log"))),
        limits: quick().limits,
        host: Arc::new(Recorder::default()),
        on_exit: Box::new(|_| {}),
    };
    let err = Sidecar::start(spawn, "2026.10").unwrap_err();
    assert_eq!(
        err.message(),
        "bridge protocol mismatch (host 1, sidecar 2)"
    );
}

#[test]
fn a_cli_command_streams_output_and_exits_with_its_code() {
    let rig = rig("echo", quick());
    let params = json!({"kind": "cli", "name": "say", "argv": ["three", "--times", "2"]});
    let answer = rig
        .supervisor
        .call("command.invoke", &params, Duration::from_secs(20))
        .unwrap();
    assert_eq!(answer, json!({"code": 3}));
    let writes: Vec<String> = rig
        .host
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "console.write")
        .map(|(_, a)| a["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(writes.concat(), "three three\n");
    let usage = json!({"kind": "cli", "name": "say", "argv": ["--bogus"]});
    let answer = rig
        .supervisor
        .call("command.invoke", &usage, Duration::from_secs(20))
        .unwrap();
    assert_eq!(answer, json!({"code": 2}));
}

#[test]
fn emitted_events_go_to_the_host_and_subscriptions_receive_deliveries() {
    let rig = rig("echo", quick());
    assert_eq!(
        rig.supervisor.tool("emit", &json!({}), &json!({})).unwrap(),
        "0"
    );
    let calls = rig.host.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|(m, a)| m == "events.emit" && a["name"] == "pinged"),
        "{calls:?}"
    );
    let registrations = rig.supervisor.registrations().unwrap();
    assert_eq!(registrations.subscriptions, ["other:*"]);
    let params = json!({"event": "other:pinged", "payload": {"n": 2}});
    rig.supervisor
        .call("event.deliver", &params, Duration::from_secs(20))
        .unwrap();
    let delivered = std::fs::read_to_string(rig.data.join("events.json")).unwrap();
    assert_eq!(
        delivered.trim(),
        r#"{"name": "other:pinged", "payload": {"n": 2}}"#
    );
}

#[test]
fn a_memory_providers_cli_module_is_its_command() {
    let rig = rig("memcli", quick());
    let registrations = rig.supervisor.start_now().unwrap();
    let command = &registrations.cli_commands[0];
    assert_eq!(command.name, "memcli");
    assert!(command.memory_provider);
    let params = json!({"kind": "cli", "name": "memcli", "argv": ["status"]});
    let answer = rig
        .supervisor
        .call("command.invoke", &params, Duration::from_secs(20))
        .unwrap();
    assert_eq!(answer, json!({"code": 0}));
    let writes: Vec<String> = rig
        .host
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "console.write")
        .map(|(_, a)| a["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(writes.concat(), "memcli status: ok\n");
}
