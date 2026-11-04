//! Hosted connectors against a fake Nous connector gateway (spec:
//! connectors, Remote connector tools through the search bridge; Connector
//! calls run through the dispatch pipeline; Multiple accounts per
//! connector; `manage_connections` tool; Connection flows require the user;
//! Reconnect and rename semantics).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::Config;
use pmt_core::cancel::CancelToken;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use pmt_tools::budget::ResultBudget;
use pmt_tools::connectors::fake::{Fake, tool};
use pmt_tools::connectors::manage::{Card, CardAnswer, CardTarget, Manager, Unavailable};
use pmt_tools::connectors::{Connectors, Gateway};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::dispatch::{AllowAll, HookCall, NoObserver, PreDecision, SeamFuture, ToolHooks};
use pmt_tools::probe::ProbeCache;
use pmt_tools::search::RemoteFuture;
use pmt_tools::variants::RouteTraits;
use pmt_tools::{
    Builtins, DispatchSetup, Dispatcher, RegisteredTool, Selection, Status, SurfaceInputs,
    ToolCall, ToolDescriptor, ToolRegistry, ToolResult, ToolSurface, Toolset,
};
use serde_json::{Value, json};

struct Echo;

impl ToolHandler for Echo {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move { Ok(ToolResult::ok(format!("ran with {arguments}"))) })
    }
}

/// Every tool name the hooks saw.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl ToolHooks for Recording {
    fn pre_tool_call<'a>(&'a self, call: HookCall<'a>) -> SeamFuture<'a, PreDecision> {
        self.0.lock().unwrap().push(call.tool.to_owned());
        Box::pin(async { PreDecision::Continue })
    }
}

/// `read_file` in `core` plus a local deferred tool about sending mail.
fn registry() -> ToolRegistry {
    let read = RegisteredTool::new(
        ToolDescriptor::new(
            "read_file",
            "file",
            "Read a file.",
            json!({"type": "object"}),
        ),
        Owner::Builtin,
        Arc::new(Echo),
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
    let owner = Owner::Mcp("mail".to_owned());
    registry
        .register_toolsets(
            &owner,
            vec![Toolset::new("mcp-mail", "", &[]).owned_by(owner.clone())],
        )
        .unwrap();
    let local = ToolDescriptor::new(
        "mcp__mail__send_email",
        "mcp-mail",
        "Send an email with the local mail server.",
        json!({"type": "object"}),
    );
    registry
        .register(
            RegisteredTool::new(local, owner, Arc::new(Echo)),
            &CapabilitySet::default(),
        )
        .unwrap();
    registry
}

fn gateway(fake: &Fake) -> Gateway {
    Gateway::new(
        &fake.url(),
        pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap(),
        Arc::new(|| Some("nous-token".to_owned())),
    )
}

async fn dispatcher(remote: Option<Arc<Connectors>>, hooks: Arc<dyn ToolHooks>) -> Dispatcher {
    let registry = registry();
    let mut config = Config::default();
    config.tools.tool_search.mode = pmt_config::schema::ToolSearchMode::On;
    let selection =
        Selection::resolve(&registry, &["core".to_owned(), "mcp-mail".to_owned()], &[]).unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let mut surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config: &config,
        probes: &ProbeCache::new(),
        route: &RouteTraits::default().with_context_window(Some(200_000)),
    })
    .await;
    if let Some(remote) = remote {
        surface = surface.with_remote(remote, &config.tools.tool_search);
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    Dispatcher::new(DispatchSetup {
        session_id: "s1".to_owned(),
        surface: Arc::new(surface),
        capabilities: Arc::new(capabilities),
        hooks,
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

fn parsed(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|e| panic!("{e}: {}", result.text_content()))
}

fn mail_tools() -> Vec<pmt_tools::connectors::wire::ToolSchema> {
    vec![
        tool("gmail", "GMAIL_SEND_EMAIL", "Send an email from Gmail."),
        tool("notion", "NOTION_CREATE_PAGE", "Create a Notion page."),
    ]
}

#[tokio::test]
async fn connector_tools_never_enter_the_tool_array() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::new(Recording::default()),
    )
    .await;
    let serialized = dispatcher.surface().serialized();
    let array: Vec<Value> = serde_json::from_str(serialized).unwrap();
    assert!(
        array
            .iter()
            .all(|t| !t["name"].as_str().unwrap().starts_with("connectors__")),
        "{serialized}"
    );
    let names: Vec<&str> = dispatcher.surface().names().collect();
    assert_eq!(
        names,
        ["read_file", "tool_search", "tool_describe", "tool_call"]
    );
    assert!(
        serialized.contains("mcp__mail__send_email"),
        "the local listing stays"
    );
    // Building the surface asks the gateway nothing.
    assert_eq!(fake.request_count().await, 0);
}

#[tokio::test]
async fn a_slow_gateway_degrades_to_local_results_without_an_error() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    fake.delay_search(Duration::from_secs(5));
    let gateway = gateway(&fake).with_search_timeout(Duration::from_millis(200));
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway)),
        Arc::new(Recording::default()),
    )
    .await;
    let started = std::time::Instant::now();
    let found = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["send an email"]}),
    )
    .await;
    assert_eq!(found.status, Status::Ok, "{}", found.text_content());
    assert!(started.elapsed() < Duration::from_secs(4));
    let found = parsed(&found);
    assert_eq!(
        found["results"][0]["tools"],
        json!(["mcp__mail__send_email"]),
        "{found}"
    );
    assert!(!found.to_string().contains("connectors__"));
}

#[tokio::test]
async fn hits_rank_with_local_tools_and_the_limit_caps_the_whole_group() {
    let fake = Fake::start(&["gmail", "notion"], mail_tools()).await;
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::new(Recording::default()),
    )
    .await;
    let found = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["send email gmail"]}),
    )
    .await;
    let found = parsed(&found);
    let tools = found["results"][0]["tools"].as_array().unwrap();
    assert_eq!(tools[0], "connectors__gmail__send_email", "{found}");
    assert!(tools.contains(&json!("mcp__mail__send_email")), "{found}");
    assert_eq!(
        found["tools"]["connectors__gmail__send_email"]["source"],
        "connectors"
    );
    let capped = call(
        &dispatcher,
        "tool_search",
        &json!({"queries": ["send email"], "limit": 1}),
    )
    .await;
    assert_eq!(
        parsed(&capped)["results"][0]["tools"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_name_collision_keeps_the_conventional_slug() {
    let tools = vec![
        tool("gmail", "send_email", "Literal slug."),
        tool("gmail", "GMAIL_SEND_EMAIL", "Conventional slug."),
    ];
    let fake = Fake::start(&["gmail"], tools).await;
    fake.add_account("gmail", None, "me@example.org", "active");
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::new(Recording::default()),
    )
    .await;
    let found = parsed(&call(&dispatcher, "tool_search", &json!({"queries": ["slug"]})).await);
    assert_eq!(
        found["results"][0]["tools"],
        json!(["connectors__gmail__send_email"])
    );
    assert_eq!(
        found["tools"]["connectors__gmail__send_email"]["description"],
        "Conventional slug."
    );
    let ran = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "connectors__gmail__send_email", "arguments": {"to": "a"}}]}),
    )
    .await;
    assert_eq!(ran.status, Status::Ok, "{}", ran.text_content());
    assert_eq!(fake.executed()[0]["tool"], "GMAIL_SEND_EMAIL");
}

#[tokio::test]
async fn describe_fetches_schemas_and_calls_retry_the_literal_slug_in_input_order() {
    let tools = vec![
        tool("gmail", "send_email", "Send mail (literal slug)."),
        tool("notion", "NOTION_CREATE_PAGE", "Create a page."),
    ];
    let fake = Fake::start(&["gmail", "notion"], tools).await;
    fake.add_account("gmail", None, "me@example.org", "active");
    fake.add_account("notion", None, "me@example.org", "active");
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::new(Recording::default()),
    )
    .await;
    let described = parsed(
        &call(
            &dispatcher,
            "tool_describe",
            &json!({"names": ["connectors__notion__create_page", "connectors__x__y"]}),
        )
        .await,
    );
    assert_eq!(
        described["tools"]["connectors__notion__create_page"]["description"],
        "Create a page."
    );
    assert_eq!(described["not_found"], json!(["connectors__x__y"]));
    let ran = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [
            {"name": "connectors__gmail__send_email", "arguments": {"to": "a"}},
            {"name": "connectors__notion__create_page", "arguments": {}}
        ]}),
    )
    .await;
    let ran = parsed(&ran);
    assert_eq!(ran[0]["name"], "connectors__gmail__send_email");
    assert_eq!(ran[0]["status"], "ok", "{ran}");
    assert_eq!(ran[1]["name"], "connectors__notion__create_page");
    let slugs: Vec<Value> = fake.executed().iter().map(|c| c["tool"].clone()).collect();
    assert_eq!(
        slugs,
        [
            json!("GMAIL_SEND_EMAIL"),
            json!("send_email"),
            json!("NOTION_CREATE_PAGE")
        ]
    );
}

#[tokio::test]
async fn an_unlinked_account_is_connection_required_through_the_whole_pipeline() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    let hooks = Arc::new(Recording::default());
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::clone(&hooks) as _,
    )
    .await;
    let result = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "connectors__gmail__send_email", "arguments": {"to": "a"}}]}),
    )
    .await;
    assert_eq!(result.status, Status::Error);
    let body = parsed(&result);
    assert_eq!(body["error"], "CONNECTION_REQUIRED");
    assert_eq!(body["connector"], "gmail");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("manage_connections") && message.contains("\"connect\""),
        "{message}"
    );
    assert!(!result.text_content().contains("https://connect.example"));
    assert_eq!(
        result.metadata["provenance"],
        json!({"source": "connector:gmail", "trust": "untrusted"})
    );
    assert_eq!(
        hooks.0.lock().unwrap().as_slice(),
        ["connectors__gmail__send_email"]
    );
    let tool = dispatcher
        .surface()
        .remote_tool("connectors__gmail__send_email")
        .unwrap();
    assert_eq!(tool.descriptor.effect, pmt_tools::Effect::External);
    assert!(!tool.descriptor.idempotent);
    // Only through the bridge: a direct call is an unknown tool.
    let direct = call(&dispatcher, "connectors__gmail__send_email", &json!({})).await;
    assert_eq!(
        direct.text_content(),
        "unknown tool: connectors__gmail__send_email"
    );
}

#[tokio::test]
async fn several_named_accounts_need_an_alias_and_results_name_the_account() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    fake.add_account("gmail", Some("home"), "me@home.org", "active");
    fake.add_account("gmail", Some("work"), "me@work.org", "active");
    let dispatcher = dispatcher(
        Some(Connectors::new(gateway(&fake))),
        Arc::new(Recording::default()),
    )
    .await;
    let found = parsed(&call(&dispatcher, "tool_search", &json!({"queries": ["gmail"]})).await);
    let described = parsed(
        &call(
            &dispatcher,
            "tool_describe",
            &json!({"names": ["connectors__gmail__send_email"]}),
        )
        .await,
    );
    let entry = &described["tools"]["connectors__gmail__send_email"];
    assert!(
        entry["description"]
            .as_str()
            .unwrap()
            .ends_with("Accounts: home, work."),
        "{entry}"
    );
    assert_eq!(
        entry["parameters"]["properties"]["connector_alias"]["enum"],
        json!(["home", "work"])
    );
    assert!(
        found["tools"]["connectors__gmail__send_email"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("connector_alias"))
    );
    let bare = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "connectors__gmail__send_email", "arguments": {"to": "a"}}]}),
    )
    .await;
    assert_eq!(bare.status, Status::Error);
    assert!(
        bare.text_content().contains("home, work"),
        "{}",
        bare.text_content()
    );
    let unknown = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "connectors__gmail__send_email", "arguments": {"connector_alias": "play"}}]}),
    )
    .await;
    assert!(
        unknown.text_content().contains("home, work"),
        "{}",
        unknown.text_content()
    );
    assert!(fake.executed().is_empty());
    let ran = call(
        &dispatcher,
        "tool_call",
        &json!({"calls": [{"name": "connectors__gmail__send_email", "arguments": {"to": "a", "connector_alias": "work"}}]}),
    )
    .await;
    assert_eq!(parsed(&ran)["account"], "work", "{}", ran.text_content());
}

/// A card answering every target the same way, after `delay`.
struct ScriptedCard {
    answer: CardAnswer,
    delay: Duration,
    shown: Mutex<Vec<CardTarget>>,
    on_show: Option<Box<dyn Fn() + Send + Sync>>,
}

impl ScriptedCard {
    fn new(answer: CardAnswer) -> Self {
        Self {
            answer,
            delay: Duration::ZERO,
            shown: Mutex::new(Vec::new()),
            on_show: None,
        }
    }
}

impl Card for ScriptedCard {
    fn show<'a>(&'a self, targets: &'a [CardTarget]) -> RemoteFuture<'a, Vec<CardAnswer>> {
        self.shown.lock().unwrap().extend(targets.iter().cloned());
        if let Some(on_show) = &self.on_show {
            on_show();
        }
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            vec![self.answer; targets.len()]
        })
    }
}

fn manager(fake: &Fake) -> Manager {
    Manager::new(Ok(gateway(fake)), Duration::from_secs(5)).with_poll(Duration::from_millis(20))
}

#[tokio::test]
async fn a_chat_without_a_card_gets_a_link_and_nothing_is_awaited() {
    let fake = Fake::start(&["notion"], mail_tools()).await;
    let manager = manager(&fake);
    let card = ScriptedCard::new(CardAnswer::Done);
    let result = manager
        .call(
            &json!({"action": "connect", "connectors": ["notion"]}),
            Some(&card),
        )
        .await;
    let body = parsed(&result);
    let outcome = &body["results"][0];
    assert_eq!(outcome["connector"], "notion");
    assert_eq!(outcome["status"], "not_connected");
    assert!(
        outcome["connect_url"]
            .as_str()
            .unwrap()
            .starts_with("https://connect.example/notion/")
    );
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("never open it yourself")
    );
    assert!(
        card.shown.lock().unwrap().is_empty(),
        "no card on a surface without cards"
    );
    // `display` there is `status`.
    let display = parsed(&manager.call(&json!({"action": "display"}), None).await);
    assert_eq!(display["connectors"][0]["connector"], "notion");
}

#[tokio::test]
async fn a_card_waits_for_the_user_and_never_returns_the_link() {
    let fake = Arc::new(Fake::start(&["gmail"], mail_tools()).await);
    let manager = manager(&fake).with_cards();
    let mut card = ScriptedCard::new(CardAnswer::Done);
    card.delay = Duration::from_secs(30);
    let completing = Arc::clone(&fake);
    card.on_show = Some(Box::new(move || completing.complete_on_read()));
    let result = manager
        .call(
            &json!({"action": "connect", "connectors": [{"name": "gmail", "alias": "work"}]}),
            Some(&card),
        )
        .await;
    let text = result.text_content();
    assert!(!text.contains("https://"), "{text}");
    let body = parsed(&result);
    assert_eq!(body["results"][0]["status"], "connected", "{body}");
    assert_eq!(body["tell_user"], "the new account is named 'work'");
    assert_eq!(card.shown.lock().unwrap()[0].alias.as_deref(), Some("work"));
}

#[tokio::test]
async fn a_skipped_or_unfinished_card_tells_the_model_not_to_ask_again() {
    let fake = Fake::start(&["gmail", "notion"], mail_tools()).await;
    let card = ScriptedCard::new(CardAnswer::Skipped);
    let manager = manager(&fake).with_cards();
    let body = parsed(
        &manager
            .call(
                &json!({"action": "connect", "connectors": ["gmail"]}),
                Some(&card),
            )
            .await,
    );
    assert_eq!(body["results"][0]["status"], "skipped");
    assert!(body["note"].as_str().unwrap().contains("do not ask again"));
    let mut slow = ScriptedCard::new(CardAnswer::Done);
    slow.delay = Duration::from_secs(30);
    let short = Manager::new(Ok(gateway(&fake)), Duration::from_millis(200))
        .with_poll(Duration::from_millis(20))
        .with_cards();
    let body = parsed(
        &short
            .call(
                &json!({"action": "connect", "connectors": ["notion"]}),
                Some(&slow),
            )
            .await,
    );
    assert_eq!(body["results"][0]["status"], "not_connected");
}

#[tokio::test]
async fn reconnect_repairs_only_broken_accounts_and_keeps_history() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    fake.add_account("gmail", Some("home"), "me@home.org", "active");
    let old = fake.add_account("gmail", Some("work"), "me@work.org", "expired");
    fake.complete_on_read();
    let manager = manager(&fake).with_cards();
    let card = ScriptedCard::new(CardAnswer::Done);
    let body = parsed(
        &manager
            .call(
                &json!({"action": "reconnect", "connectors": ["gmail"]}),
                Some(&card),
            )
            .await,
    );
    let results = body["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|r| r["account"] == "home" && r["status"] == "connected"),
        "{body}"
    );
    assert!(
        results
            .iter()
            .any(|r| r["alias"] == "work" && r["status"] == "connected"),
        "{body}"
    );
    let accounts = fake.accounts();
    let work: Vec<_> = accounts
        .iter()
        .filter(|a| a.alias.as_deref() == Some("work"))
        .collect();
    assert_eq!(work.len(), 2);
    let retired = work.iter().find(|a| a.connection_id == old).unwrap();
    assert!(retired.disabled, "the old account is kept, retired");
    assert!(work.iter().any(|a| a.connection_id != old && a.is_active()));
    // An already connected account is left alone without force.
    let again = parsed(
        &manager
            .call(
                &json!({"action": "reconnect", "connectors": [{"name": "gmail", "alias": "home"}]}),
                Some(&card),
            )
            .await,
    );
    assert!(
        again["results"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("already connected")
    );
    let status = parsed(
        &manager
            .call(&json!({"action": "status", "connectors": ["gmail"]}), None)
            .await,
    );
    let rows = status["connectors"][0]["accounts"].as_array().unwrap();
    assert!(rows.iter().any(|r| r["status"] == "retired"), "{status}");
}

#[tokio::test]
async fn rename_sets_an_alias_and_reports_a_portal_without_the_route() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    fake.add_account("gmail", None, "me@home.org", "active");
    let manager = manager(&fake);
    let body = parsed(
        &manager
            .call(
                &json!({"action": "rename", "connectors": [{"name": "gmail", "alias": "me@home.org", "to": "home"}]}),
                None,
            )
            .await,
    );
    assert!(
        body["message"].as_str().unwrap().contains("'home'"),
        "{body}"
    );
    assert_eq!(fake.accounts()[0].alias.as_deref(), Some("home"));
    fake.without_rename();
    let refused = manager
        .call(
            &json!({"action": "rename", "connectors": [{"name": "gmail", "to": "personal"}]}),
            None,
        )
        .await;
    assert_eq!(refused.status, Status::Error);
    assert_eq!(
        refused.text_content(),
        "this Nous portal cannot rename connector accounts yet"
    );
}

#[tokio::test]
async fn wrong_target_kinds_and_removals_are_refused_without_a_request() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    let manager = manager(&fake);
    let install = manager
        .call(&json!({"action": "install", "connectors": ["gmail"]}), None)
        .await;
    assert_eq!(install.status, Status::Error);
    let text = install.text_content();
    assert!(
        text.contains(r#"{"name": "gmail", "mcp": true}"#) && text.contains("\"connect\""),
        "{text}"
    );
    let removal = manager
        .call(
            &json!({"action": "disconnect", "connectors": ["gmail"]}),
            None,
        )
        .await;
    assert!(
        removal
            .text_content()
            .contains("prometheus connectors disconnect")
    );
    assert_eq!(fake.request_count().await, 0);
}

#[tokio::test]
async fn without_a_sign_in_hosted_actions_point_to_the_user_and_send_nothing() {
    let fake = Fake::start(&["gmail"], mail_tools()).await;
    let manager = Manager::new(Err(Unavailable::SignedOut), Duration::from_secs(1));
    let result = manager
        .call(&json!({"action": "connect", "connectors": ["gmail"]}), None)
        .await;
    assert!(result.text_content().contains("prometheus auth login nous"));
    assert_eq!(fake.request_count().await, 0);
}
