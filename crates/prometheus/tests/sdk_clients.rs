//! The embeddable client SDKs (spec: workspace-architecture, Embeddable
//! client SDKs) against a real daemon: the Python package (on the python
//! bridge's test interpreter), the TypeScript package (on Node 22) and the
//! Rust crate run a turn, and each refuses a daemon speaking a protocol
//! major it does not support before sending any request. Needs uv on PATH
//! (see `pmt_pybridge::testkit`) and Node 22.

mod common;
mod eval_common;

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use pmt_core::runtime::{Role, RuntimeRecord, run_dir};
use pmt_rpc::discovery::{DaemonRecord, Endpoint};
use serde_json::Value;

use self::common::{append_config, mock_home, ok, run};

const BINARY: &str = env!("CARGO_BIN_EXE_prometheus");

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A home whose mock model reads a file with a tool, then answers.
fn scripted_home() -> tempfile::TempDir {
    let home = mock_home();
    let profile = home.path().join("profiles/default");
    let notes = profile.join("workspace/notes.txt");
    std::fs::create_dir_all(notes.parent().unwrap()).unwrap();
    std::fs::write(&notes, "remember the milk").unwrap();
    std::fs::write(
        profile.join("script.toml"),
        format!(
            r#"
[[rule]]
user = "list files"
[[rule.tool_calls]]
name = "read_file"
arguments = {{ path = {:?} }}

[[rule]]
tool_result = "remember"
text = "the notes say remember the milk"
"#,
            notes.to_str().unwrap()
        ),
    )
    .unwrap();
    append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"script.toml\"\n",
    );
    home
}

/// A program's process, isolated like every test command.
fn program(home: &Path, exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    common::isolate_user_home(&mut cmd, home);
    cmd.env_remove("PMT_HOME").env_remove("PROMETHEUS_HOME");
    cmd
}

fn python(home: &Path) -> PathBuf {
    pmt_pybridge::testkit::seed(home);
    pmt_pybridge::runtime::find(home, pmt_pybridge::testkit::VERSION)
        .expect("the test interpreter")
        .exe
}

/// What a client script printed (one JSON object).
fn printed(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("not JSON: {stdout}"))
}

/// The scenario's result: tool and message events, then usage totals.
fn assert_embedded_turn(result: &Value) {
    let types: Vec<&str> = result["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    let at = |kind: &str| {
        types
            .iter()
            .position(|t| *t == kind)
            .unwrap_or_else(|| panic!("no {kind}: {types:?}"))
    };
    assert!(at("tool.call") < at("tool.result"), "{types:?}");
    assert!(at("tool.result") < at("assistant.message"), "{types:?}");
    assert_eq!(result["exit_reason"], "completed", "{result}");
    assert!(
        result["usage"]
            .as_object()
            .is_some_and(|usage| !usage.is_empty()),
        "usage totals: {result}"
    );
}

const PYTHON_EMBEDDING: &str = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
from prometheus_sdk import Client
session = Client.connect(home=sys.argv[2], binary=sys.argv[3]).sessions.create(toolsets=["file"])
types = [event.type for event in session.prompt("list files")]
result = session.result
print(json.dumps({"types": types, "exit_reason": result.exit_reason, "usage": result.usage,
                  "history": len(session.history())}))
"#;

/// Scenario: Python embedding.
#[test]
fn a_python_script_runs_a_turn_and_gets_its_events_and_usage() {
    let home = scripted_home();
    let python = python(home.path());
    let _daemon = common::daemon::start(home.path(), &[]);
    let out = program(home.path(), &python)
        .arg("-c")
        .arg(PYTHON_EMBEDDING)
        .arg(repo().join("sdk/python"))
        .arg(home.path())
        .arg(BINARY)
        .output()
        .unwrap();
    let result = printed(&out);
    assert_embedded_turn(&result);
    assert!(result["history"].as_u64().unwrap() >= 2, "{result}");
}

const NODE_EMBEDDING: &str = r#"
const [sdk, home, binary] = process.argv.slice(1);
const { Client } = await import(sdk);
const client = await Client.connect({ home, binary });
const session = await client.sessions.create({ toolsets: ["file"] });
const types = [];
for await (const event of session.prompt("list files")) types.push(event.type);
console.log(JSON.stringify({ types, exit_reason: session.result.exitReason, usage: session.result.usage }));
client.close();
"#;

fn node(home: &Path) -> Command {
    let mut cmd = program(home, Path::new("node"));
    cmd.args(["--input-type=module", "-e", NODE_EMBEDDING, "--"]);
    cmd
}

#[test]
fn a_node_program_runs_a_turn_and_gets_its_events_and_usage() {
    let home = scripted_home();
    let _daemon = common::daemon::start(home.path(), &[]);
    let sdk = repo().join("sdk/typescript/src/index.ts");
    let out = node(home.path())
        .arg(&sdk)
        .arg(home.path())
        .arg(BINARY)
        .output()
        .unwrap();
    assert_embedded_turn(&printed(&out));
}

/// The `FakeProvider`'s profile: an OpenAI-compatible endpoint that reports
/// usage and keeps every request body.
fn provider_home(server: &eval_common::FakeProvider) -> tempfile::TempDir {
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n[providers.acme]\nbase_url = \"http://127.0.0.1:{}/v1\"\nwire_format = \"chat_completions\"\nauth = \"none\"\nmax_retries = 0\n\n[budgets]\nallow_unpriced = true\n",
            server.port
        ),
    )
    .unwrap();
    home
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_session_with_an_overlay_leaves_no_transcript() {
    let server = eval_common::FakeProvider::start("Short answer.");
    let home = provider_home(&server);
    let daemon = common::daemon::start(home.path(), &[]);
    let options = pmt_sdk::Connect {
        home: Some(home.path().to_path_buf()),
        binary: Some(BINARY.into()),
        ..pmt_sdk::Connect::default()
    };
    let client = pmt_sdk::Client::connect(&options).await.unwrap();
    let create = pmt_sdk::Create::default()
        .ephemeral(true)
        .system_prompt_overlay("OVERLAY-ANSWER-TERSELY");
    let session = client.create(&create).await.unwrap();
    let mut kinds = Vec::new();
    let turn = session
        .prompt("PROMPT-PRIVATE-QUESTION", |event| {
            kinds.push(event.kind.clone());
        })
        .await
        .unwrap();
    assert_eq!(turn.final_response.as_deref(), Some("Short answer."));
    assert!(turn.usage.is_some(), "{turn:?}");
    assert!(kinds.iter().any(|k| k == "assistant.message"), "{kinds:?}");
    // Multi-turn use reads the live transcript.
    assert!(session.history().await.unwrap().len() >= 2);
    assert!(
        server
            .bodies()
            .iter()
            .any(|b| b.contains("OVERLAY-ANSWER-TERSELY") && b.contains("PROMPT-PRIVATE-QUESTION")),
        "the overlay rides with the call"
    );
    let listed = ok(&run(home.path(), &["--json", "sessions", "list"]));
    assert!(!listed.contains(&session.id), "hidden: {listed}");
    let id = session.id.clone();
    drop(client);
    drop(daemon);
    // Stopped: the transcript is gone from the store, and no other file
    // (trajectory, log, blob) holds the prompt or the overlay; the spend
    // is still on the ledger.
    assert!(common::session_messages(home.path(), &id).is_empty());
    let found = ok(&run(
        home.path(),
        &["--json", "sessions", "search", "PRIVATE"],
    ));
    assert!(!found.contains(&id), "{found}");
    for file in common::files_under(&home.path().join("profiles/default")) {
        if file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("state.db"))
        {
            continue;
        }
        let bytes = std::fs::read(&file).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        for secret in ["PROMPT-PRIVATE-QUESTION", "OVERLAY-ANSWER-TERSELY"] {
            assert!(!text.contains(secret), "{} keeps {secret}", file.display());
        }
    }
    let usage = ok(&run(home.path(), &["--json", "usage", "--session", &id]));
    let usage: Value = serde_json::from_str(&usage).unwrap();
    assert_eq!(
        usage["total"]["calls"], 1,
        "the spend stays on the ledger: {usage}"
    );
}

/// A home whose "daemon" (this process) speaks protocol major 2 at an
/// RPC address nothing should ever dial.
fn newer_daemon() -> (tempfile::TempDir, TcpListener) {
    let home = mock_home();
    let rpc = TcpListener::bind("127.0.0.1:0").unwrap();
    rpc.set_nonblocking(true).unwrap();
    let record = DaemonRecord {
        process: RuntimeRecord::current(Role::Daemon, vec!["default".into()]),
        endpoint: Endpoint {
            instance_id: "future".into(),
            build_id: "b".into(),
            rpc_url: format!("ws://{}/rpc", rpc.local_addr().unwrap()),
            control: "unused".into(),
            rpc_protocol: "2.0".into(),
            started_at: "2026-11-18T00:00:00Z".into(),
            health_url: None,
            features: Vec::new(),
        },
    };
    let path = run_dir(home.path()).join(Role::Daemon.record_file());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    (home, rpc)
}

fn assert_never_dialed(rpc: &TcpListener) {
    std::thread::sleep(Duration::from_millis(200));
    if let Ok((mut stream, _)) = rpc.accept() {
        let mut sent = String::new();
        let _ = stream.read_to_string(&mut sent);
        panic!("the SDK dialed the daemon: {sent:?}");
    }
}

/// Scenario: Old SDK against a new protocol.
#[tokio::test(flavor = "multi_thread")]
async fn every_sdk_refuses_a_newer_protocol_before_any_request() {
    let (home, rpc) = newer_daemon();
    let options = pmt_sdk::Connect {
        home: Some(home.path().to_path_buf()),
        binary: Some(BINARY.into()),
        ..pmt_sdk::Connect::default()
    };
    let err = pmt_sdk::Client::connect(&options).await.unwrap_err();
    assert_eq!(err.code().code, "rpc.protocol_mismatch", "{err}");
    assert_never_dialed(&rpc);

    let python = python(home.path());
    let out = program(home.path(), &python)
        .arg("-c")
        .arg(
            "import sys\nsys.path.insert(0, sys.argv[1])\nfrom prometheus_sdk import Client, ProtocolMismatch\ntry:\n    Client.connect(home=sys.argv[2], binary=sys.argv[3])\nexcept ProtocolMismatch as err:\n    print('mismatch:', err)\n",
        )
        .arg(repo().join("sdk/python"))
        .arg(home.path())
        .arg(BINARY)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.starts_with("mismatch:") && said.contains("2.0"),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_never_dialed(&rpc);

    let out = program(home.path(), Path::new("node"))
        .args([
            "--input-type=module",
            "-e",
            "const [sdk, home, binary] = process.argv.slice(1); const { Client, ProtocolMismatch } = await import(sdk); try { await Client.connect({ home, binary }); } catch (err) { if (err instanceof ProtocolMismatch) console.log(`mismatch: ${err.message}`); else throw err; }",
            "--",
        ])
        .arg(repo().join("sdk/typescript/src/index.ts"))
        .arg(home.path())
        .arg(BINARY)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.starts_with("mismatch:") && said.contains("2.0"),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_never_dialed(&rpc);
    let _ = std::fs::remove_file(run_dir(home.path()).join(Role::Daemon.record_file()));
}
