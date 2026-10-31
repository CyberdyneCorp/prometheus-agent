//! The listings over a registry with registered tools (the binary's own
//! built-in tools arrive with their tool groups), compared with snapshots
//! in `tests/snapshots/`.

use std::path::Path;
use std::sync::Arc;

use pmt_config::Config;
use pmt_config::schema::ApprovalMode;
use pmt_core::capability::{BASELINE, Capability, CapabilitySet};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, PathClaim, ToolHandler};
use pmt_tools::probe::{FnProbe, ProbeCache};
use pmt_tools::{Builtins, RegisteredTool, Selection, ToolDescriptor, ToolRegistry, ToolResult};
use serde_json::{Value, json};

use super::{
    Inventory, explain, list, record_session_surface, render_explain, render_list, render_show,
    show, slash_toolsets,
};

struct Ok;

impl ToolHandler for Ok {
    fn call<'a>(&'a self, _: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async { Result::Ok(ToolResult::ok("ok")) })
    }
}

fn tool(name: &str, toolset: &str) -> ToolDescriptor {
    ToolDescriptor::new(
        name,
        toolset,
        format!("The {name} tool."),
        json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    )
}

/// `terminal` as the terminal group declares it, `write_file`, a browser
/// tool whose runtime is missing and `web_extract` needing the network.
fn registry() -> ToolRegistry {
    let mut terminal = ToolDescriptor::new(
        "terminal",
        "terminal",
        "Run a shell command.",
        json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
    );
    terminal.capabilities = vec![Capability::parse("process.spawn:*").unwrap()];
    let mut write = tool("write_file", "file");
    write.effect = pmt_tools::Effect::Workspace;
    write.claims.writes = vec![PathClaim::Argument("/path".into())];
    let mut web = tool("web_extract", "web");
    web.capabilities = vec![Capability::parse("net.connect:*").unwrap()];
    let browser = RegisteredTool::new(
        tool("browser_exec", "browser"),
        Owner::Builtin,
        Arc::new(Ok),
    )
    .with_probe(Arc::new(FnProbe::new("browser", |_| {
        Err("browser not found".to_owned())
    })));
    let tools = vec![
        RegisteredTool::new(terminal, Owner::Builtin, Arc::new(Ok)),
        RegisteredTool::new(write, Owner::Builtin, Arc::new(Ok)),
        RegisteredTool::new(web, Owner::Builtin, Arc::new(Ok)),
        RegisteredTool::new(tool("x_search", "x_search"), Owner::Builtin, Arc::new(Ok)),
        browser,
    ];
    ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()))
}

fn no_network() -> CapabilitySet {
    CapabilitySet::parse(BASELINE.iter().copied().chain(["process.spawn:*"]))
        .unwrap()
        .without(&Capability::parse("net.connect:*").unwrap())
}

fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

async fn with<T>(config: &Config, body: impl AsyncFnOnce(&Inventory<'_>) -> T) -> T {
    let registry = registry();
    let probes = ProbeCache::new();
    let caps = no_network();
    let inventory = Inventory {
        registry: &registry,
        config,
        capabilities: &caps,
        probes: &probes,
    };
    body(&inventory).await
}

#[tokio::test]
async fn tools_list_reports_one_reason_per_excluded_tool() {
    let rows = with(&Config::default(), async |inv| list(inv).await.unwrap()).await;
    let reason = |name: &str| {
        rows.iter()
            .find(|r| r.name == name)
            .unwrap()
            .exposure
            .clone()
    };
    assert_eq!(reason("terminal"), "exposed");
    assert_eq!(reason("web_extract"), "missing_capability:net.connect:*");
    assert_eq!(reason("browser_exec"), "unavailable:browser not found");
    assert_eq!(reason("x_search"), "not_in_toolset");
    snapshot("tools-list.txt", &render_list(&rows));
}

#[tokio::test]
async fn explaining_a_tool() {
    let mut config = Config::default();
    config.approvals.mode = ApprovalMode::Smart;
    let terminal = with(&config, async |inv| explain(inv, "terminal").await.unwrap()).await;
    let text = render_explain(&terminal);
    assert!(text.contains("capabilities = process.spawn:*\n"), "{text}");
    assert!(text.contains("effect = external\n"));
    assert!(text.contains("approval_mode = smart\n"));
    snapshot("tools-explain-terminal.txt", &text);

    let write = with(&config, async |inv| {
        explain(inv, "write_file").await.unwrap()
    })
    .await;
    assert!(
        write
            .approval_rules
            .iter()
            .any(|r| r.contains("protected instruction files")),
        "{:?}",
        write.approval_rules
    );
    let err = with(&config, async |inv| {
        explain(inv, "todo_list").await.unwrap_err()
    })
    .await;
    assert_eq!(err.message(), "unknown tool: todo_list");
    assert_eq!(err.exit_code(), 2);
}

#[tokio::test]
async fn toolset_show_lists_resolved_tools_with_availability() {
    let shown = with(&Config::default(), async |inv| {
        show(inv, "core").await.unwrap()
    })
    .await;
    assert_eq!(shown.tools.len(), 27);
    let names: Vec<&str> = shown.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, pmt_tools::toolset::CORE);
    snapshot("toolsets-show-core-fixture.txt", &render_show(&shown));
    let err = with(&Config::default(), async |inv| {
        show(inv, "brwser").await.unwrap_err()
    })
    .await;
    assert_eq!(
        err.message(),
        "unknown toolset 'brwser' (did you mean 'browser'?)"
    );
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn slash_listing_marks_active_toolsets() {
    let registry = registry();
    let selection =
        Selection::resolve(&registry, &["core".to_owned(), "web".to_owned()], &[]).unwrap();
    let text = slash_toolsets(&registry, &selection).unwrap();
    let line = |name: &str| {
        text.lines()
            .find(|l| l.split_whitespace().nth(usize::from(l.starts_with('*'))) == Some(name))
            .unwrap_or_else(|| panic!("{name} listed"))
            .to_owned()
    };
    assert!(
        line("core").starts_with("* core  (27 tools)  [active]"),
        "{text}"
    );
    assert!(line("web").starts_with("* web  (2 tools)  [active]"));
    assert_eq!(line("video"), "  video  (1 tool)  [default-off]");
    assert_eq!(line("coding"), "  coding  (23 tools)  [posture]");
    assert!(line("file").starts_with("  file"));
}

#[tokio::test]
async fn a_session_records_the_surface_it_built() {
    let root = tempfile::tempdir().unwrap();
    let home = pmt_config::Home::open(root.path()).unwrap();
    let profile = home
        .profile(&pmt_config::ProfileName::default_profile())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        profile,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    let store = pmt_store::Store::open(&scope).unwrap();
    let session = store
        .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
        .unwrap();

    let registry = registry();
    let rows = with(&Config::default(), async |inv| {
        let (_, surface) = inv.surface(&["core".to_owned()]).await.unwrap();
        record_session_surface(&store, session.id, &registry, &surface).unwrap();
        list(inv).await.unwrap()
    })
    .await;

    let recorded = store.tool_surface(session.id).unwrap().unwrap();
    assert_eq!(
        recorded.tools, rows,
        "the same verdicts as the live listing"
    );
}
