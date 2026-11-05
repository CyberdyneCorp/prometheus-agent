//! Tool search progressive disclosure (spec: tool-system, Tool search
//! progressive disclosure; Tool search listing, ranking and limits): which
//! tools leave the frozen array, and the bridge tools that find, describe
//! and run them through the full pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pmt_config::schema::{Config, ToolSearchMode};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use pmt_tools::budget::ResultBudget;
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::dispatch::{AllowAll, NoHooks, NoObserver};
use pmt_tools::probe::ProbeCache;
use pmt_tools::variants::RouteTraits;
use pmt_tools::{
    Builtins, DispatchSetup, Dispatcher, RegisteredTool, Selection, Status, SurfaceInputs,
    ToolCall, ToolDescriptor, ToolRegistry, ToolResult, ToolSurface, Toolset,
};
use serde_json::{Value, json};

/// Counts its runs and echoes its arguments.
struct Counting(Arc<AtomicUsize>);

impl ToolHandler for Counting {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(ToolResult::ok(format!("ran with {arguments}"))) })
    }
}

/// A `core` built-in, plus MCP server `gh` with `count` tools named
/// `mcp__gh__<verb>_<n>`, every one counting into `runs`.
fn registry(count: usize, runs: &Arc<AtomicUsize>) -> ToolRegistry {
    let read = RegisteredTool::new(
        ToolDescriptor::new(
            "read_file",
            "file",
            "Read a file.",
            json!({"type": "object"}),
        ),
        Owner::Builtin,
        Arc::new(Counting(Arc::clone(runs))),
    );
    let builtins = Builtins::with(
        vec![
            Toolset::new("file", "", &["read_file"]),
            Toolset::new("core", "", &["read_file"]),
        ],
        vec![read],
    )
    .unwrap();
    let mut registry = ToolRegistry::new("default", Arc::new(builtins));
    let owner = Owner::Mcp("gh".to_owned());
    registry
        .register_toolsets(
            &owner,
            vec![Toolset::new("mcp-gh", "", &[]).owned_by(owner.clone())],
        )
        .unwrap();
    for i in 0..count {
        let verb = ["create_issue", "delete_repo", "list_pulls"][i % 3];
        let parameters = json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"]
        });
        let descriptor = ToolDescriptor::new(
            format!("mcp__gh__{verb}_{i}"),
            "mcp-gh",
            format!("GitHub: {} number {i}.", verb.replace('_', " ")),
            parameters,
        );
        registry
            .register(
                RegisteredTool::new(
                    descriptor,
                    owner.clone(),
                    Arc::new(Counting(Arc::clone(runs))),
                ),
                &CapabilitySet::default(),
            )
            .unwrap();
    }
    let docs = Owner::Mcp("docs".to_owned());
    registry
        .register_toolsets(
            &docs,
            vec![Toolset::new("mcp-docs", "", &[]).owned_by(docs.clone())],
        )
        .unwrap();
    let search = ToolDescriptor::new(
        "mcp__docs__search",
        "mcp-docs",
        "Search docs.",
        json!({"type": "object"}),
    );
    registry
        .register(
            RegisteredTool::new(search, docs, Arc::new(Counting(Arc::clone(runs)))),
            &CapabilitySet::default(),
        )
        .unwrap();
    registry
}

fn config(mode: ToolSearchMode) -> Config {
    let mut config = Config::default();
    config.tools.tool_search.mode = mode;
    config
}

async fn dispatcher(registry: &ToolRegistry, toolsets: &[&str], config: &Config) -> Dispatcher {
    let selection = Selection::resolve(
        registry,
        &toolsets.iter().map(|&t| t.to_owned()).collect::<Vec<_>>(),
        &[],
    )
    .unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
        route: &RouteTraits::default().with_context_window(Some(200_000)),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    Dispatcher::new(DispatchSetup {
        session_id: "s1".to_owned(),
        surface: Arc::new(surface),
        capabilities: Arc::new(capabilities),
        hooks: Arc::new(NoHooks),
        gate: Arc::new(AllowAll),
        observer: Arc::new(NoObserver),
        budget: ResultBudget::new(
            config.tools.result_budget.clone(),
            None,
            dir.path().join("spill"),
            Redactor::default(),
        ),
        reporter: Arc::new(CrashReporter::new(
            dir.path().join("crash"),
            Redactor::default(),
        )),
        default_timeout: Duration::from_secs(30),
        concurrency: 4,
    })
}

async fn call(dispatcher: &Dispatcher, tool: &str, arguments: &Value) -> ToolResult {
    dispatcher
        .dispatch(
            ToolCall::new("c1", tool, &arguments.to_string()),
            &CancelToken::new(),
        )
        .await
}

fn names(dispatcher: &Dispatcher) -> Vec<String> {
    dispatcher.surface().names().map(str::to_owned).collect()
}

#[tokio::test]
async fn only_the_bridge_tools_are_exposed_until_a_deferred_tool_is_called_through_them() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(6, &runs);
    let dispatcher = dispatcher(&registry, &["core", "mcp-gh"], &config(ToolSearchMode::On)).await;
    assert_eq!(
        names(&dispatcher),
        ["read_file", "tool_search", "tool_describe", "tool_call"]
    );
    // The deferred tools are only named in the search tool's listing.
    let listing = &dispatcher.surface().definitions()[1].description;
    assert!(
        listing.contains("- mcp__gh__create_issue_0: GitHub: create issue number 0"),
        "{listing}"
    );

    let found = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["create issue"]}),
    )
    .await;
    let found: Value = serde_json::from_str(&found.text_content()).unwrap();
    let first = found["results"][0]["tools"][0].as_str().unwrap().to_owned();
    assert!(first.starts_with("mcp__gh__create_issue_"), "{found}");
    assert_eq!(found["tools"][&first]["required"], json!(["title"]));
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let described = call(
        &dispatcher,
        "tool_describe",
        &json!({"names": [first, "nope"]}),
    )
    .await;
    let described: Value = serde_json::from_str(&described.text_content()).unwrap();
    assert_eq!(described["not_found"], json!(["nope"]));
    assert_eq!(
        described["tools"][&first]["parameters"]["required"],
        json!(["title"])
    );

    let ran = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": first, "arguments": {"title": "bug"}}]}),
    )
    .await;
    assert_eq!(ran.status, Status::Ok);
    assert_eq!(ran.text_content(), r#"ran with {"title":"bug"}"#);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_bridged_call_is_validated_against_the_deferred_tools_schema() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(3, &runs);
    let dispatcher = dispatcher(&registry, &["core", "mcp-gh"], &config(ToolSearchMode::On)).await;
    let calls = json!({"calls": "[{\"name\": \"mcp__gh__create_issue_0\", \"arguments\": {}}]"});
    let refused = call(&dispatcher, "tool_call", &calls).await;
    assert_eq!(refused.status, Status::Error);
    assert!(
        refused
            .text_content()
            .contains("invalid arguments for mcp__gh__create_issue_0"),
        "{}",
        refused.text_content()
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn out_of_scope_bridge_call_is_an_unknown_tool() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(3, &runs);
    // A session whose toolsets exclude the `gh` server's tools.
    let dispatcher = dispatcher(
        &registry,
        &["core", "mcp-docs"],
        &config(ToolSearchMode::On),
    )
    .await;
    assert_eq!(dispatcher.surface().deferred().len(), 1);
    let ran = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "mcp__gh__delete_repo_1", "arguments": {"title": "x"}}]}),
    )
    .await;
    assert_eq!(ran.text_content(), "unknown tool: mcp__gh__delete_repo_1");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_batch_naming_a_listed_tool_is_corrected() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(3, &runs);
    let dispatcher = dispatcher(&registry, &["core", "mcp-gh"], &config(ToolSearchMode::On)).await;
    let calls = json!({"calls": [
        {"name": "read_file", "arguments": {}},
        {"name": "mcp__gh__create_issue_0", "arguments": {"title": "x"}}
    ]});
    let refused = call(&dispatcher, "tool_call", &calls).await;
    assert!(
        refused
            .text_content()
            .contains("call read_file directly. Valid shape:"),
        "{}",
        refused.text_content()
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auto_defers_only_when_the_deferrable_schemas_outgrow_the_threshold() {
    let runs = Arc::new(AtomicUsize::new(0));
    let small = registry(3, &runs);
    let auto = config(ToolSearchMode::Auto);
    let direct = dispatcher(&small, &["core", "mcp-gh"], &auto).await;
    assert!(direct.surface().catalog().is_none());
    assert!(names(&direct).contains(&"mcp__gh__create_issue_0".to_owned()));

    let large = registry(200, &runs);
    let deferred = dispatcher(&large, &["core", "mcp-gh"], &auto).await;
    assert_eq!(deferred.surface().deferred().len(), 200);
    assert!(names(&deferred).contains(&"tool_search".to_owned()));

    let off = dispatcher(&large, &["core", "mcp-gh"], &config(ToolSearchMode::Off)).await;
    assert!(off.surface().catalog().is_none());
    assert_eq!(names(&off).len(), 201);
}

#[tokio::test]
async fn limit_clamp() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(90, &runs);
    let dispatcher = dispatcher(&registry, &["core", "mcp-gh"], &config(ToolSearchMode::On)).await;
    let found = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["github"], "limit": 80}),
    )
    .await;
    let found: Value = serde_json::from_str(&found.text_content()).unwrap();
    assert_eq!(found["results"][0]["tools"].as_array().unwrap().len(), 25);
    let default = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["github", "weather"]}),
    )
    .await;
    let default: Value = serde_json::from_str(&default.text_content()).unwrap();
    assert_eq!(default["results"][0]["tools"].as_array().unwrap().len(), 5);
    assert_eq!(
        default["results"][1]["available_sources"],
        json!({"mcp-gh": 90})
    );
}

#[tokio::test]
async fn a_core_tool_is_deferred_only_when_the_defer_list_names_it() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = registry(3, &runs);
    let on = config(ToolSearchMode::On);
    let kept = dispatcher(&registry, &["core", "mcp-gh"], &on).await;
    assert!(names(&kept).contains(&"read_file".to_owned()));
    let mut listed = on.clone();
    listed.tools.tool_search.defer = Some(vec!["read_file".to_owned()]);
    let deferred = dispatcher(&registry, &["core", "mcp-gh"], &listed).await;
    assert!(!names(&deferred).contains(&"read_file".to_owned()));
    let ran = call(
        &deferred,
        "tool_call",
        &json!({"calls": [{"name": "read_file", "arguments": {}}]}),
    )
    .await;
    assert_eq!(ran.status, Status::Ok);
}
