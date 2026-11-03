//! The MCP client against the reference fixture server (spec:
//! plugin-system, MCP client servers; tool-system, MCP tool naming and
//! filtering; MCP runtime safety): stdio and streamable HTTP servers,
//! registration under `mcp-<server>`, filters, trust, crashes, restart
//! backoff, timeouts and the circuit breaker.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::schema::{McpToolFilter, McpTrust};
use pmt_core::cancel::CancelToken;
use pmt_mcp::naming::Filter;
use pmt_mcp::server::{Launcher, State};
use pmt_mcp::{Launch, Manager, ServerEntry, Settings};
use pmt_tools::descriptor::{CallContext, Effect, Executor};
use pmt_tools::{Builtins, Status, ToolRegistry, ToolResult};
use serde_json::{Value, json};

fn settings(trust: McpTrust, filter: &McpToolFilter) -> Settings {
    Settings {
        transport: "stdio",
        trust,
        filter: Filter::of(filter),
        call_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(20),
    }
}

fn stdio(args: &[&str]) -> Launcher {
    let args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
    Arc::new(move || {
        let mut command = tokio::process::Command::new(pmt_mcp::testkit::fixture());
        command.args(&args);
        Ok(Launch::Stdio(command))
    })
}

fn manager(name: &str, settings: Settings, launcher: Launcher) -> Manager {
    Manager::new(
        vec![ServerEntry {
            name: name.to_owned(),
            settings,
            launcher,
        }],
        4,
    )
}

fn registry(manager: &Manager) -> (ToolRegistry, Vec<String>) {
    let builtins = Builtins::with(Vec::new(), Vec::new()).unwrap();
    let mut registry = ToolRegistry::new("default", Arc::new(builtins));
    let toolsets = manager.register(&mut registry);
    (registry, toolsets)
}

async fn call(registry: &ToolRegistry, name: &str, arguments: Value) -> ToolResult {
    let tool = registry.get(name).unwrap();
    let Executor::Handler(handler) = &tool.executor else {
        panic!("{name} has no handler");
    };
    let context = CallContext {
        call_id: "c1".into(),
        session_id: "s1".into(),
        cancel: CancelToken::new(),
    };
    handler.call(&context, arguments).await.unwrap()
}

fn calls_logged(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |t| t.lines().count())
}

#[tokio::test]
async fn a_stdio_server_registers_its_tools_under_its_toolset_and_answers_calls() {
    let manager = manager(
        "fx",
        settings(McpTrust::Untrusted, &McpToolFilter::default()),
        stdio(&[]),
    );
    assert_eq!(manager.statuses()[0].state, State::NotConnected);
    let outcomes = manager.discover().await;
    assert_eq!(outcomes, [("fx".to_owned(), Ok(()))]);
    let (registry, toolsets) = registry(&manager);
    assert_eq!(toolsets, ["mcp-fx"]);
    let echo = registry.get("mcp__fx__echo").unwrap();
    assert_eq!(echo.descriptor.toolset, "mcp-fx");
    assert_eq!(echo.descriptor.effect, Effect::None);
    assert!(echo.descriptor.approval.is_none());
    assert!(registry.get("mcp__fx__do_thing_x").is_ok());
    // An untrusted server's tool that is not read-only asks every time.
    let add = registry.get("mcp__fx__add").unwrap();
    let rule = add.descriptor.approval.as_ref().unwrap();
    assert_eq!(rule.rule_key, "mcp:fx:add");
    let echoed = call(&registry, "mcp__fx__echo", json!({"text": "hi"})).await;
    assert_eq!(
        (echoed.status, echoed.text_content()),
        (Status::Ok, "hi".to_owned())
    );
    let failed = call(&registry, "mcp__fx__fail", json!({})).await;
    assert_eq!(
        (failed.status, failed.text_content()),
        (Status::Error, "it failed".to_owned())
    );
    assert_eq!(manager.statuses()[0].state, State::Connected);
}

#[tokio::test]
async fn include_beats_exclude_and_full_trust_asks_for_nothing() {
    let filter = McpToolFilter {
        include: Some(vec!["create_issue".into()]),
        exclude: vec!["create_issue".into(), "delete_issue".into()],
    };
    let manager = manager("gh", settings(McpTrust::Full, &filter), stdio(&[]));
    manager.discover().await;
    let (registry, _) = registry(&manager);
    let names: Vec<&str> = registry.tools().map(|t| t.name()).collect();
    assert_eq!(names, ["mcp__gh__create_issue"]);
    assert!(
        registry
            .get("mcp__gh__create_issue")
            .unwrap()
            .descriptor
            .approval
            .is_none()
    );
}

#[tokio::test]
async fn a_streamable_http_server_needs_its_bearer_token() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(pmt_mcp::fixture::serve_http(
        pmt_mcp::fixture::Fixture::default(),
        listener,
        Some("s3cret".into()),
    ));
    let http = |bearer: Option<&str>| -> Launcher {
        let (url, bearer) = (url.clone(), bearer.map(str::to_owned));
        Arc::new(move || {
            Ok(Launch::Http {
                url: url.clone(),
                client: pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap(),
                bearer: bearer.clone(),
            })
        })
    };
    let mut config = settings(McpTrust::Full, &McpToolFilter::default());
    config.transport = "http";
    config.connect_timeout = Duration::from_secs(5);
    let wrong = manager("web", config.clone(), http(Some("nope")));
    assert!(wrong.discover().await[0].1.is_err());
    let right = manager("web", config, http(Some("s3cret")));
    assert_eq!(right.discover().await[0].1, Ok(()));
    let (registry, _) = registry(&right);
    let added = call(&registry, "mcp__web__add", json!({"a": 2, "b": 3})).await;
    assert_eq!(added.text_content(), "5");
}

#[tokio::test]
async fn a_crashed_server_answers_unavailable_and_restarts_after_its_backoff() {
    let manager = manager(
        "github",
        settings(McpTrust::Full, &McpToolFilter::default()),
        stdio(&[]),
    );
    manager.discover().await;
    let (registry, _) = registry(&manager);
    let crashed = call(&registry, "mcp__github__crash", json!({})).await;
    assert_eq!(crashed.status, Status::Error);
    assert!(
        crashed
            .text_content()
            .starts_with("MCP server 'github' unavailable"),
        "{}",
        crashed.text_content()
    );
    // Within the first backoff second the server is not restarted.
    let again = call(&registry, "mcp__github__echo", json!({"text": "x"})).await;
    assert!(
        again
            .text_content()
            .starts_with("MCP server 'github' unavailable")
    );
    assert!(matches!(manager.statuses()[0].state, State::Unavailable(_)));
    let server = manager.server("github").unwrap();
    assert!(
        server
            .availability()
            .unwrap_err()
            .starts_with("mcp_server_down")
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let back = call(&registry, "mcp__github__echo", json!({"text": "back"})).await;
    assert_eq!(back.text_content(), "back");
}

#[tokio::test]
async fn three_timeouts_open_the_circuit_and_the_fourth_call_never_reaches_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let mut config = settings(McpTrust::Full, &McpToolFilter::default());
    config.call_timeout = Duration::from_millis(200);
    let manager = manager("s", config, stdio(&["--log", log.to_str().unwrap()]));
    manager.discover().await;
    let (registry, _) = registry(&manager);
    for _ in 0..3 {
        let slow = call(&registry, "mcp__s__sleep", json!({"ms": 2_000})).await;
        assert_eq!(
            slow.text_content(),
            "MCP server 's' did not answer sleep within 200 ms"
        );
    }
    let refused = call(&registry, "mcp__s__echo", json!({"text": "x"})).await;
    let text = refused.text_content();
    assert!(
        text.starts_with("MCP server 's' is unreachable after 3 consecutive failures; retry in ~"),
        "{text}"
    );
    assert!(text.ends_with("60s") || text.ends_with("59s"), "{text}");
    assert_eq!(calls_logged(&log), 3);
}

#[tokio::test]
async fn a_server_that_never_answers_initialize_times_out_and_waits_out_its_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings(McpTrust::Full, &McpToolFilter::default());
    config.connect_timeout = Duration::from_millis(500);
    let manager = manager("slow", config, stdio(&["--mode", "hang"]));
    let started = Instant::now();
    let outcome = manager.discover().await;
    assert_eq!(
        outcome[0].1,
        Err("no answer to initialize within 500 ms".to_owned())
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // The backoff is running: no second start.
    let again = Instant::now();
    assert!(manager.discover().await[0].1.is_err());
    assert!(again.elapsed() < Duration::from_millis(300));
    let crash = manager_in(dir.path(), &["--mode", "crash-on-start"]);
    let failed = crash.discover().await;
    assert!(
        failed[0].1.as_ref().unwrap_err().contains("exited"),
        "{failed:?}"
    );
}

fn manager_in(_: &Path, args: &[&str]) -> Manager {
    manager(
        "bad",
        settings(McpTrust::Full, &McpToolFilter::default()),
        stdio(args),
    )
}

#[tokio::test]
async fn discovery_starts_at_most_the_configured_number_of_servers_at_once() {
    let entry = |name: &str| {
        let mut config = settings(McpTrust::Full, &McpToolFilter::default());
        config.connect_timeout = Duration::from_millis(400);
        ServerEntry {
            name: name.to_owned(),
            settings: config,
            launcher: stdio(&["--mode", "hang"]),
        }
    };
    let one_at_a_time = Manager::new(vec![entry("a"), entry("b"), entry("c")], 1);
    let started = Instant::now();
    one_at_a_time.discover().await;
    assert!(started.elapsed() >= Duration::from_millis(1_200));
    let all_at_once = Manager::new(vec![entry("a"), entry("b"), entry("c")], 3);
    let started = Instant::now();
    all_at_once.discover().await;
    assert!(started.elapsed() < Duration::from_millis(1_100));
}
