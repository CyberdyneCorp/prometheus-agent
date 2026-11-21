//! MCP event subscriptions against the fixture server (spec: plugin-system,
//! MCP events): the three tools, once-only approval of every
//! subscription, the route and secret a subscription stores, HTTPS-only
//! delivery, and a refresh the server refuses for good.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::{McpEventRoute, McpToolFilter, SecretName, WebhookRoute};
use pmt_config::webhook_subscriptions::{self, Subscription};
use pmt_config::{
    Home, Launch as ConfigLaunch, Overrides, ProfileName, ProfileScope, mcp_event_state,
};
use pmt_core::cancel::CancelToken;
use pmt_mcp::events::{Events, route_name};
use pmt_mcp::naming::Filter;
use pmt_mcp::server::Launcher;
use pmt_mcp::{Launch, Manager, ServerEntry, Settings};
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::{Builtins, Status, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Profile {
    _home: tempfile::TempDir,
    scope: ProfileScope,
    log: std::path::PathBuf,
    notices: Arc<Mutex<Vec<(String, String)>>>,
}

fn profile() -> Profile {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let layout = Home::open(home.path()).unwrap();
    let dir = layout
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), ConfigLaunch::Other).unwrap();
    let log = home.path().join("calls.log");
    Profile {
        _home: home,
        scope,
        log,
        notices: Arc::default(),
    }
}

impl Profile {
    fn events(&self, public_url: &str) -> Events {
        let notices = Arc::clone(&self.notices);
        Events {
            scope: self.scope.clone(),
            public_url: Some(public_url.to_owned()),
            notify: Arc::new(move |to: &str, text: &str| {
                notices
                    .lock()
                    .unwrap()
                    .push((to.to_owned(), text.to_owned()));
            }),
        }
    }

    fn manager(&self, events: Option<Events>) -> Manager {
        let settings = Settings {
            filter: Filter::of(&McpToolFilter::default()),
            call_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(20),
            events,
            ..Settings::default()
        };
        let log = self.log.clone();
        let launcher: Launcher = Arc::new(move || {
            let mut command = tokio::process::Command::new(pmt_mcp::testkit::fixture());
            command.arg("--log").arg(&log);
            Ok(Launch::Stdio(command))
        });
        Manager::new(
            vec![ServerEntry {
                name: "gh".to_owned(),
                settings,
                launcher,
            }],
            4,
        )
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
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

#[tokio::test]
async fn event_tools_exist_only_for_servers_with_events() {
    let p = profile();
    let plain = p.manager(None);
    plain.discover().await;
    assert!(registry(&plain).get("mcp__gh__subscribe_event").is_err());
    let with = p.manager(Some(p.events("https://hooks.example")));
    with.discover().await;
    let registry = registry(&with);
    for tool in ["list_event_types", "subscribe_event", "unsubscribe_event"] {
        assert!(registry.get(&format!("mcp__gh__{tool}")).is_ok(), "{tool}");
    }
    let listed = call(&registry, "mcp__gh__list_event_types", json!({})).await;
    assert!(
        listed.text_content().contains("issue.opened"),
        "{}",
        listed.text_content()
    );
}

#[tokio::test]
async fn a_subscription_asks_every_time_and_stores_a_signed_route() {
    let p = profile();
    let manager = p.manager(Some(p.events("https://hooks.example/")));
    manager.discover().await;
    let registry = registry(&manager);
    let args = json!({"name": "issue.opened", "arguments": {"repo": "o/r"},
                      "instructions": "Triage the issue.", "deliver": "telegram:42"});
    let tool = registry.get("mcp__gh__subscribe_event").unwrap();
    let ask = (tool.guard.as_ref().expect("a consent guard"))("s1", &args).unwrap();
    assert!(ask.once_only, "asked every time, whatever the mode");
    assert!(
        ask.reason.contains("Triage the issue.") && ask.reason.contains("telegram:42"),
        "{}",
        ask.reason
    );

    let result = call(&registry, "mcp__gh__subscribe_event", args).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let route = route_name("gh", "issue.opened", &json!({"repo": "o/r"}));
    let subs = webhook_subscriptions::load(p.scope.dir()).unwrap();
    let sub = &subs[&route];
    assert_eq!(sub.route.deliver.as_deref(), Some("telegram:42"));
    assert!(sub.route.prompt.as_deref().unwrap().contains("{data}"));
    let secret = sub.route.secret_secret.as_ref().unwrap();
    let value = p.scope.secrets().resolve(&secret.0).unwrap();
    assert!(value.expose().starts_with("whsec_"));
    let calls = p.calls();
    assert!(
        calls.contains(&format!(
            "\"url\":\"https://hooks.example/p/default/webhook/{route}\""
        )),
        "{calls}"
    );
    let state = mcp_event_state::get(p.scope.dir(), &route);
    assert_eq!(state.subscription_id.as_deref(), Some("sub-issue.opened"));
    assert_eq!(state.cursor.as_deref(), Some("c0"));

    let gone = call(
        &registry,
        "mcp__gh__unsubscribe_event",
        json!({"name": "issue.opened", "arguments": {"repo": "o/r"}}),
    )
    .await;
    assert_eq!(gone.status, Status::Ok, "{}", gone.text_content());
    assert!(
        webhook_subscriptions::load(p.scope.dir())
            .unwrap()
            .is_empty()
    );
    assert!(p.scope.secrets().resolve(&secret.0).is_err());
    assert!(p.calls().contains("events/unsubscribe"));
}

#[tokio::test]
async fn plain_http_delivery_is_refused() {
    let p = profile();
    let manager = p.manager(Some(p.events("http://hooks.example")));
    manager.discover().await;
    let result = call(
        &registry(&manager),
        "mcp__gh__subscribe_event",
        json!({"name": "issue.opened"}),
    )
    .await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("https://"),
        "{}",
        result.text_content()
    );
    assert!(
        webhook_subscriptions::load(p.scope.dir())
            .unwrap()
            .is_empty()
    );
    assert!(!p.calls().contains("events/subscribe"));
}

/// A connection refreshes every stored subscription; one the server
/// refuses for good is removed and its target told.
#[tokio::test]
async fn a_refresh_refused_for_good_removes_the_route_and_notifies() {
    let p = profile();
    for (args, deliver) in [
        (json!({"repo": "o/r"}), "log"),
        (json!({"revoked": true}), "telegram:7"),
    ] {
        let name = route_name("gh", "issue.opened", &args);
        let secret = webhook_subscriptions::secret_name(&name);
        p.scope
            .secrets()
            .store()
            .set(&secret, &pmt_core::secret::SecretString::new("whsec_AAAA"))
            .unwrap();
        let mut all = webhook_subscriptions::load(p.scope.dir()).unwrap();
        all.insert(
            name.clone(),
            Subscription {
                created_at: "2026-11-01T00:00:00Z".into(),
                generated_secret: true,
                route: WebhookRoute {
                    secret_secret: Some(SecretName(secret)),
                    deliver: Some(deliver.into()),
                    mcp_event: Some(McpEventRoute {
                        server: "gh".into(),
                        name: "issue.opened".into(),
                        arguments: args.to_string(),
                    }),
                    ..WebhookRoute::default()
                },
            },
        );
        webhook_subscriptions::save(p.scope.dir(), &all).unwrap();
        mcp_event_state::update(p.scope.dir(), &name, |s| {
            s.granted_at = Some(0);
            s.refresh_before = Some("1970-01-02T00:00:00Z".into());
        })
        .unwrap();
    }
    let manager = p.manager(Some(p.events("https://hooks.example")));
    manager.discover().await;
    let kept = route_name("gh", "issue.opened", &json!({"repo": "o/r"}));
    let settled = || {
        webhook_subscriptions::load(p.scope.dir()).unwrap().len() == 1
            && !p.notices.lock().unwrap().is_empty()
            && mcp_event_state::get(p.scope.dir(), &kept).granted_at != Some(0)
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !settled() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no refresh pass: {}",
            p.calls()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        webhook_subscriptions::load(p.scope.dir())
            .unwrap()
            .contains_key(&kept)
    );
    let renewed = mcp_event_state::get(p.scope.dir(), &kept);
    assert_eq!(
        renewed.refresh_before.as_deref(),
        Some("2099-01-01T00:00:00Z"),
        "renewed before expiry"
    );
    let notices = p.notices.lock().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, "telegram:7");
    assert!(notices[0].1.contains("access revoked"), "{}", notices[0].1);
}
