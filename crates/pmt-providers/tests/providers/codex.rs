//! The Codex app-server runtime (spec: model-providers, Codex app-server
//! runtime) against the fake Codex CLI `pmt-codex-fixture`, one copy per
//! test so each has its own request log.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pmt_core::id::SessionId;
use pmt_providers::StreamEvent;
use pmt_providers::codex::{RuntimeApprovals, RuntimeAsk};
use pmt_providers::request::{Message, Role};
use pmt_providers::resolve::{Target, Trace};
use serde_json::Value;

use crate::support::{Env, caller, request};

/// A copy of the fixture in a directory of its own.
fn install() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let file = dir
        .path()
        .join(format!("codex{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_pmt-codex-fixture"), &file).unwrap();
    (dir, file)
}

fn log(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("codex-fixture.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn requests<'a>(log: &'a [Value], method: &str) -> Vec<&'a Value> {
    log.iter()
        .filter(|e| e["method"] == method)
        .map(|e| &e["params"])
        .collect()
}

fn config(runtime: &str, codex: &Path) -> String {
    format!(
        "[model]\nopenai_runtime = \"{runtime}\"\ncodex_bin = '{}'\n",
        codex.display()
    )
}

/// Records each ask and answers `allow`.
struct Gate {
    allow: bool,
    asked: Mutex<Vec<RuntimeAsk>>,
    workdir: Option<PathBuf>,
}

impl RuntimeApprovals for Gate {
    fn ask(&self, ask: RuntimeAsk) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        self.asked.lock().unwrap().push(ask);
        Box::pin(async move { self.allow })
    }

    fn workdir(&self, _session: &str) -> Option<PathBuf> {
        self.workdir.clone()
    }
}

async fn events(
    env: &Env,
    gate: Option<Arc<Gate>>,
    request: &pmt_providers::Request,
    session: Option<SessionId>,
) -> Vec<StreamEvent> {
    let route = env.route("openai-codex", "default").await;
    let mut caller = caller();
    if let Some(gate) = gate {
        caller = caller.with_runtime_approvals(gate);
    }
    let ctx = session.map(|s| pmt_providers::cost::CallContext::new(s, "main"));
    let mut stream = caller.send(&route, request, ctx.as_ref()).await.unwrap();
    let events = stream.collect().await;
    assert!(stream.failure().is_none(), "{:?}", stream.failure());
    events
}

fn text(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_turn_streams_codex_messages_and_the_actions_it_performed() {
    let (dir, codex) = install();
    let env = Env::new(&config("codex_app_server", &codex), &[]);
    let route = env.route("openai-codex", "default").await;
    assert!(
        route.base_url.starts_with("codex:///"),
        "{}",
        route.base_url
    );
    assert_eq!(route.billing_source(), "subscription:openai-codex/external");
    let gate = Arc::new(Gate {
        allow: true,
        asked: Mutex::default(),
        workdir: None,
    });
    let events = events(
        &env,
        Some(Arc::clone(&gate)),
        &request("please run it"),
        None,
    )
    .await;
    assert_eq!(text(&events), "codex saw: please run it");
    assert!(events.contains(&StreamEvent::ReasoningDelta {
        text: "thinking".into()
    }));
    let performed: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ExternalCall { .. }))
        .collect();
    assert_eq!(
        performed,
        [&StreamEvent::ExternalCall {
            id: "cmd-1".into(),
            name: "terminal".into(),
            arguments: r#"{"command":"echo hi","workdir":"/work"}"#.into(),
            output: "hi\n".into(),
            ok: true,
        }]
    );
    // The command went through the session's gate as a `terminal` call.
    let asked = gate.asked.lock().unwrap();
    assert_eq!(
        (asked[0].tool, asked[0].call_id.as_str()),
        ("terminal", "cmd-1")
    );
    assert_eq!(asked[0].arguments["command"], "echo hi");
    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage { usage } => Some(*usage),
        _ => None,
    });
    let usage = usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens
        ),
        (100, 20, 7)
    );
    assert!(log(dir.path()).iter().any(|e| e["approval"] == "accept"));
}

#[tokio::test]
async fn a_session_thread_runs_in_its_workspace_and_asks_before_commands() {
    let (dir, codex) = install();
    let env = Env::new(&config("codex_app_server", &codex), &[]);
    let workspace = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let gate = Arc::new(Gate {
        allow: true,
        asked: Mutex::default(),
        workdir: Some(workspace.clone()),
    });
    let session = SessionId::new();
    events(&env, Some(gate), &request("hello"), Some(session)).await;
    pmt_providers::codex::close(&session.to_string());
    let log = log(dir.path());
    let started = log.iter().find_map(|e| e["cwd"].as_str()).unwrap();
    assert_eq!(Path::new(started).canonicalize().unwrap(), workspace);
    let thread = requests(&log, "thread/start")[0];
    assert_eq!(thread["cwd"], workspace.to_string_lossy().as_ref());
    // Fail closed: Codex asks before every command it does not deem
    // read-only, inside a sandbox Prometheus chose.
    assert_eq!(thread["approvalPolicy"], "untrusted");
    assert_eq!(thread["sandbox"], "workspace-write");
}

#[tokio::test]
async fn without_a_gate_codex_approvals_are_declined() {
    let (dir, codex) = install();
    let env = Env::new(&config("codex_app_server", &codex), &[]);
    let events = events(&env, None, &request("run something"), None).await;
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::ExternalCall { name, ok: false, .. } if name == "terminal"
    )));
    assert!(log(dir.path()).iter().any(|e| e["approval"] == "decline"));
}

#[tokio::test]
async fn one_server_and_thread_serve_a_session_and_the_persona_goes_once() {
    let (dir, codex) = install();
    let env = Env::new(&config("codex_app_server", &codex), &[]);
    let session = SessionId::new();
    let first = request("first");
    events(&env, None, &first, Some(session)).await;
    let mut second = first.clone();
    second
        .messages
        .push(Message::text(Role::Assistant, "codex saw: first"));
    second.messages.push(Message::text(Role::User, "second"));
    let events = events(&env, None, &second, Some(session)).await;
    assert_eq!(text(&events), "codex saw: second");
    pmt_providers::codex::close(&session.to_string());
    let log = log(dir.path());
    assert_eq!(log.iter().filter(|e| e.get("start").is_some()).count(), 1);
    let threads = requests(&log, "thread/start");
    assert_eq!(threads.len(), 1);
    assert_eq!(
        threads[0]["developerInstructions"],
        "You are a careful assistant.\n\nProject: demo."
    );
    let turns = requests(&log, "turn/start");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["input"][0]["text"], "first");
    // Only what the thread has not seen (its own answer it holds).
    assert_eq!(turns[1]["input"][0]["text"], "second");
}

#[tokio::test]
async fn the_auto_runtime_keeps_prometheus_on_the_route() {
    let (_dir, codex) = install();
    let env = Env::new(&config("auto", &codex), &[]);
    let target = Target {
        provider: "openai-api".to_owned(),
        ..Target::default()
    };
    let route = env
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await;
    // `auto` resolves the provider's own endpoint (here: no key yet).
    assert!(route.map_or(true, |r| !r.base_url.starts_with("codex:")));
}

/// Scenario: Codex binary missing.
#[tokio::test]
async fn a_missing_codex_binary_fails_the_route_naming_model_codex_bin() {
    let missing = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-codex");
    let env = Env::new(&config("codex_app_server", &missing), &[]);
    let target = Target {
        provider: "openai-codex".to_owned(),
        ..Target::default()
    };
    let err = env
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.external_cli_missing");
    assert!(
        err.hint().is_some_and(|h| h.contains("model.codex_bin")),
        "{err:?}"
    );
}
