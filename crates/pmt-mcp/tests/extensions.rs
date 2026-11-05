//! The MCP client's M2 extensions against the fixture server (spec:
//! tool-system, MCP resource and prompt utility tools; MCP tool list
//! changes respect the frozen surface; MCP result sanitization; MCP media
//! results; plugin-system, MCP sampling; MCP elicitation; MCP tool costs
//! and spend limits; MCP config watch, recycling and discovery wait).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::{McpSamplingConfig, McpToolFilter, McpTrust};
use pmt_core::cancel::CancelToken;
use pmt_mcp::costs::Costs;
use pmt_mcp::host::{ClientHost, ElicitAnswer, Elicitation, HostFuture, Sampled, Sampling};
use pmt_mcp::naming::Filter;
use pmt_mcp::requests::SamplingPolicy;
use pmt_mcp::server::{Launcher, Recycle, State};
use pmt_mcp::{Launch, Manager, ServerEntry, Settings};
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::result::Content;
use pmt_tools::{Builtins, Status, ToolRegistry, ToolResult};
use serde_json::{Map, Value, json};

/// A host that records what servers asked of it.
#[derive(Default)]
struct TestHost {
    media_dir: PathBuf,
    images: bool,
    elicit: Option<ElicitAnswer>,
    sampled: AtomicUsize,
    changed: Mutex<Vec<String>>,
    spent: Mutex<f64>,
}

impl ClientHost for TestHost {
    fn store_media(&self, _: &str, bytes: &[u8], _: &str) -> Result<String, String> {
        let path = self.media_dir.join("block.png");
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn accepts_images(&self, _: &str) -> bool {
        self.images
    }

    fn tools_changed(&self, server: &str) {
        self.changed.lock().unwrap().push(server.to_owned());
    }

    fn elicit(&self, _: &str, _: Elicitation) -> HostFuture<'_, ElicitAnswer> {
        let answer = self.elicit.clone().unwrap_or(ElicitAnswer::Decline);
        Box::pin(async move { answer })
    }

    fn sample(&self, _: &str, request: Sampling) -> HostFuture<'_, Result<Sampled, String>> {
        self.sampled.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(Sampled {
                model: "mock".into(),
                text: format!("answer to {}", request.messages[0].1),
            })
        })
    }

    fn spent(&self, _: &str, _: &str) -> f64 {
        *self.spent.lock().unwrap()
    }

    fn add_spent(&self, _: &str, _: &str, amount: f64) {
        *self.spent.lock().unwrap() += amount;
    }
}

fn settings(trust: McpTrust) -> Settings {
    Settings {
        trust,
        filter: Filter::of(&McpToolFilter::default()),
        call_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(20),
        ..Settings::default()
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

fn manager(settings: Settings, args: &[&str], host: Arc<TestHost>) -> Manager {
    let host: Arc<dyn pmt_mcp::ClientHost> = host;
    Manager::with_host(
        vec![ServerEntry {
            name: "fx".to_owned(),
            settings,
            launcher: stdio(args),
        }],
        4,
        &host,
    )
}

fn registry(manager: &Manager) -> ToolRegistry {
    let builtins = Builtins::with(Vec::new(), Vec::new()).unwrap();
    let mut registry = ToolRegistry::new("default", Arc::new(builtins));
    manager.register(&mut registry);
    registry
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

fn names(registry: &ToolRegistry) -> Vec<String> {
    registry.tools().map(|t| t.name().to_owned()).collect()
}

#[tokio::test]
async fn tools_only_server_gets_no_utility_tools() {
    let manager = manager(settings(McpTrust::Untrusted), &[], Arc::default());
    manager.discover().await;
    let names = names(&registry(&manager));
    for utility in [
        "list_resources",
        "read_resource",
        "list_prompts",
        "get_prompt",
    ] {
        assert!(!names.contains(&format!("mcp__fx__{utility}")), "{names:?}");
    }
}

#[tokio::test]
async fn resource_and_prompt_utilities_follow_the_capabilities_and_the_filter() {
    let mut filtered = settings(McpTrust::Untrusted);
    filtered.filter = Filter::of(&McpToolFilter {
        include: None,
        exclude: vec!["get_*".into()],
    });
    let manager = manager(filtered, &["--offers", "resources,prompts"], Arc::default());
    manager.discover().await;
    let registry = registry(&manager);
    let names = names(&registry);
    for utility in ["list_resources", "read_resource", "list_prompts"] {
        assert!(names.contains(&format!("mcp__fx__{utility}")), "{names:?}");
    }
    assert!(!names.contains(&"mcp__fx__get_prompt".to_owned()));
    let listed = call(&registry, "mcp__fx__list_resources", json!({})).await;
    assert!(
        listed.text_content().contains("fixture://readme"),
        "{}",
        listed.text_content()
    );
    let read = call(
        &registry,
        "mcp__fx__read_resource",
        json!({"uri": "fixture://readme"}),
    )
    .await;
    assert_eq!(read.status, Status::Ok);
    assert!(read.text_content().contains("readme text"));
    assert!(
        !read
            .text_content()
            .chars()
            .any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c))
    );
    let prompts = call(&registry, "mcp__fx__list_prompts", json!({})).await;
    assert!(prompts.text_content().contains("greet"));
}

#[tokio::test]
async fn smuggled_instructions_removed_and_reserved_meta_dropped() {
    let manager = manager(settings(McpTrust::Full), &["--extras"], Arc::default());
    manager.discover().await;
    let result = call(&registry(&manager), "mcp__fx__smuggle", json!({})).await;
    let text = result.text_content();
    assert!(text.starts_with("The weather is sunny."), "{text}");
    assert!(
        !text
            .chars()
            .any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c))
    );
    let meta = result.metadata["meta"].as_object().unwrap();
    let keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    assert_eq!(keys, ["com.example/handoff"]);
    assert!(
        text.contains("meta: {\"com.example/handoff\":\"next\"}"),
        "{text}"
    );
}

#[tokio::test]
async fn screenshot_from_an_mcp_server() {
    let dir = tempfile::tempdir().unwrap();
    for images in [true, false] {
        let host = Arc::new(TestHost {
            media_dir: dir.path().to_path_buf(),
            images,
            ..TestHost::default()
        });
        let manager = manager(settings(McpTrust::Full), &["--extras"], host);
        manager.discover().await;
        let result = call(&registry(&manager), "mcp__fx__image", json!({})).await;
        let path = dir.path().join("block.png");
        assert_eq!(
            result.text_content(),
            format!("MEDIA:{} (image/png)", path.display())
        );
        assert_eq!(&std::fs::read(&path).unwrap()[1..4], b"PNG");
        let attached = result
            .content
            .iter()
            .any(|c| matches!(c, Content::Media { mime, .. } if mime == "image/png"));
        assert_eq!(attached, images, "a vision route gets the image part");
    }
}

#[test]
fn a_block_over_50_mib_is_refused_naming_it() {
    let huge = "A".repeat(50 * 1024 * 1024 / 3 * 4 + 8);
    let result: rmcp::model::CallToolResult = serde_json::from_value(json!({
        "content": [
            {"type": "text", "text": "before"},
            {"type": "image", "data": huge, "mimeType": "image/png"},
        ],
    }))
    .unwrap();
    let converted = pmt_mcp::result::converted(&result, &pmt_mcp::NoHost, "s1");
    assert_eq!(converted.status, Status::Error);
    assert_eq!(
        converted.text_content(),
        "content block 2 (image/png) is larger than 50 MiB"
    );
}

#[tokio::test]
async fn burst_of_notifications() {
    let host = Arc::new(TestHost::default());
    let manager = manager(settings(McpTrust::Full), &["--extras"], Arc::clone(&host));
    manager.discover().await;
    let frozen = registry(&manager);
    let generation = manager.generation();
    call(&frozen, "mcp__fx__change_tools", json!({})).await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let server = manager.server("fx").unwrap();
    let refreshes = server.refreshes();
    assert!((1..=2).contains(&refreshes), "{refreshes} refreshes");
    assert_ne!(manager.generation(), generation);
    assert_eq!(*host.changed.lock().unwrap(), ["fx"]);
    // The running session's array is the one it froze.
    assert!(frozen.get("mcp__fx__delete_issue").is_ok());
    assert!(frozen.get("mcp__fx__new_tool").is_err());
    let gone = call(&frozen, "mcp__fx__delete_issue", json!({"id": 1})).await;
    assert_eq!(
        gone.text_content(),
        "tool 'mcp__fx__delete_issue' is no longer offered by MCP server 'fx'"
    );
    // The next session gets the new list.
    let next = names(&registry(&manager));
    assert!(next.contains(&"mcp__fx__new_tool".to_owned()));
    assert!(!next.contains(&"mcp__fx__delete_issue".to_owned()));
}

#[tokio::test]
async fn rate_limit_enforced() {
    let host = Arc::new(TestHost::default());
    let mut trusted = settings(McpTrust::Full);
    trusted.sampling = SamplingPolicy::of(&McpSamplingConfig::default(), McpTrust::Full);
    assert_eq!(trusted.sampling.max_rpm, 10);
    let manager = manager(trusted, &["--extras"], Arc::clone(&host));
    manager.discover().await;
    let result = call(&registry(&manager), "mcp__fx__sample", json!({"n": 11})).await;
    let text = result.text_content();
    let answers: Vec<&str> = text.lines().collect();
    assert_eq!(answers.len(), 11, "{text}");
    assert!(answers[..10].iter().all(|a| a.starts_with("ok ")), "{text}");
    assert!(
        answers[10].starts_with("error ") && answers[10].contains("rate limit"),
        "{text}"
    );
    assert_eq!(
        host.sampled.load(Ordering::SeqCst),
        10,
        "no model call for the 11th"
    );
}

#[tokio::test]
async fn sampling_is_off_for_untrusted_servers_by_default() {
    let host = Arc::new(TestHost::default());
    let manager = manager(
        settings(McpTrust::Untrusted),
        &["--extras"],
        Arc::clone(&host),
    );
    manager.discover().await;
    let result = call(&registry(&manager), "mcp__fx__sample", json!({"n": 1})).await;
    assert!(
        result.text_content().starts_with("error "),
        "{}",
        result.text_content()
    );
    assert_eq!(host.sampled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unattended_elicitation_declined() {
    let manager = manager(settings(McpTrust::Full), &["--extras"], Arc::default());
    manager.discover().await;
    let result = call(&registry(&manager), "mcp__fx__ask", json!({})).await;
    assert_eq!(result.status, Status::Ok, "the tool call continues");
    assert!(
        result.text_content().contains("\"action\":\"decline\""),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn an_answered_elicitation_returns_the_values_once() {
    let mut values = Map::new();
    values.insert("repo".into(), json!("prometheus"));
    let host = Arc::new(TestHost {
        elicit: Some(ElicitAnswer::Accept(values)),
        ..TestHost::default()
    });
    let manager = manager(settings(McpTrust::Full), &["--extras"], host);
    manager.discover().await;
    let result = call(&registry(&manager), "mcp__fx__ask", json!({})).await;
    let text = result.text_content();
    assert!(
        text.contains("\"action\":\"accept\"") && text.contains("prometheus"),
        "{text}"
    );
}

#[tokio::test]
async fn budget_crossing_asks_even_when_trusted() {
    let host = Arc::new(TestHost::default());
    *host.spent.lock().unwrap() = 90.0;
    let mut renders = settings(McpTrust::Full);
    let costs = serde_json::from_value(json!({"render": 20})).unwrap();
    renders.costs = Costs::parse(&costs, 100.0, "credits").unwrap();
    let manager = manager(renders, &["--extras"], Arc::clone(&host));
    manager.discover().await;
    let registry = registry(&manager);
    let render = registry.get("mcp__fx__render").unwrap();
    assert!(render.descriptor.approval.is_none());
    let guard = render.guard.as_ref().unwrap();
    let asked = guard("s1", &json!({})).unwrap();
    assert!(asked.once_only);
    assert!(
        asked.reason.contains("about 20 credits"),
        "{}",
        asked.reason
    );
    assert!(
        asked
            .reason
            .contains("90 credits used in this conversation (limit 100)"),
        "{}",
        asked.reason
    );
    // Under the limit a trusted server's costed tool asks nothing, and a
    // call that ran is charged.
    *host.spent.lock().unwrap() = 10.0;
    assert!(guard("s1", &json!({})).is_none());
    call(&registry, "mcp__fx__render", json!({"prompt": "cat"})).await;
    assert!((*host.spent.lock().unwrap() - 30.0).abs() < 1e-9);
}

#[tokio::test]
async fn recycled_server_restarts_on_demand() {
    let mut idle = settings(McpTrust::Full);
    idle.recycle = Recycle {
        idle: Some(Duration::from_secs(1)),
        max_lifetime: None,
    };
    let manager = manager(idle, &[], Arc::default());
    manager.discover().await;
    let registry = registry(&manager);
    let before = names(&registry);
    assert_eq!(
        call(&registry, "mcp__fx__echo", json!({"text": "a"}))
            .await
            .status,
        Status::Ok
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        manager.statuses()[0].state,
        State::Idle,
        "stopped after its idle time"
    );
    assert!(
        manager.server("fx").unwrap().availability().is_ok(),
        "its tools stay offered"
    );
    let again = call(&registry, "mcp__fx__echo", json!({"text": "b"})).await;
    assert_eq!(
        (again.status, again.text_content()),
        (Status::Ok, "b".to_owned())
    );
    assert_eq!(manager.statuses()[0].state, State::Connected);
    assert_eq!(
        names(&registry),
        before,
        "the session's tool list is unchanged"
    );
}

#[tokio::test]
async fn a_lazy_server_registers_from_the_schema_cache_and_starts_on_first_call() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("fx.json");
    let log = dir.path().join("calls.log");
    let log_arg = log.to_string_lossy().into_owned();
    let mut first = settings(McpTrust::Full);
    first.schema_cache = Some(cache.clone());
    let warm = manager(first.clone(), &["--log", &log_arg], Arc::default());
    warm.discover().await;
    warm.stop();
    assert!(cache.is_file());
    let mut lazy = first;
    lazy.lazy = true;
    let manager = manager(lazy, &["--log", &log_arg], Arc::default());
    manager.discover().await;
    assert_eq!(
        manager.statuses()[0].state,
        State::Idle,
        "not started by discovery"
    );
    let registry = registry(&manager);
    assert!(registry.get("mcp__fx__echo").is_ok());
    let echoed = call(&registry, "mcp__fx__echo", json!({"text": "hi"})).await;
    assert_eq!(echoed.text_content(), "hi");
    assert_eq!(manager.statuses()[0].state, State::Connected);
}

#[tokio::test]
async fn session_start_waits_only_for_the_discovery_window() {
    let manager = manager(
        settings(McpTrust::Full),
        &["--mode", "hang"],
        Arc::default(),
    );
    let started = std::time::Instant::now();
    let outcomes = manager
        .discover_within(Some(Duration::from_millis(300)))
        .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(outcomes[0].1, Err("still connecting".to_owned()));
}
