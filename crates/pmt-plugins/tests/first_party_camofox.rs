//! The `camofox` catalog plugin (spec: plugin-system, Catalog plugins for
//! Home Assistant and Camofox) against a fake Camofox REST server: each
//! session drives its own tab with its session id as the Camofox user,
//! pages come back as snapshots, and only web URLs open.

mod common;

use std::sync::{Arc, Mutex};

use common::{Fake, Profile, Seen};
use pmt_core::cancel::CancelToken;
use pmt_plugins::host::ports::{HttpPort, HttpRequest, HttpResponse, PortFuture};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::tools::SessionCalls;
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::{Builtins, Status, ToolRegistry, ToolResult};
use serde_json::{Value, json};

/// A fake Camofox server: `METHOD path body` of every request.
#[derive(Default)]
struct Server {
    requests: Mutex<Vec<String>>,
}

impl HttpPort for Server {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        let path = request
            .url
            .strip_prefix("http://localhost:9377")
            .unwrap_or(&request.url)
            .to_owned();
        let body = request
            .body
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        self.requests
            .lock()
            .unwrap()
            .push(format!("{} {path} {body}", request.method));
        let answer = if request.method == "POST" && path == "/tabs" {
            json!({"tabId": "tab-1"})
        } else if path.contains("/snapshot") {
            json!({"url": "https://example.org/", "snapshot": "heading \"Example\"\nlink \"More\" [e1]"})
        } else {
            json!({"ok": true})
        };
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: answer.to_string().into_bytes(),
            })
        })
    }
}

struct Loaded {
    server: Arc<Server>,
    registry: ToolRegistry,
    _runtime: Runtime,
    _profile: Profile,
}

fn load() -> Loaded {
    let mut profile = Profile::new("p");
    profile.install_shipped("first-party", "camofox", "first-party-camofox");
    let fake = Arc::new(Fake {
        seen: Arc::new(Seen::default()),
        secrets: std::collections::BTreeMap::new(),
        settings: Mutex::default(),
        credentials: Mutex::default(),
    });
    fake.set("camofox", "url", "\"http://localhost:9377/\"");
    let server = Arc::new(Server::default());
    let mut ports = fake.ports();
    ports.http = Arc::clone(&server) as Arc<dyn HttpPort>;
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &ports);
    let mut registry = ToolRegistry::new("p", Arc::new(Builtins::standard(Vec::new()).unwrap()));
    let calls: SessionCalls = Arc::new(|session: &str| CallInfo {
        session_id: Some(session.to_owned()),
        surface: "tui".to_owned(),
        cwd: None,
    });
    let call = CallInfo {
        session_id: None,
        surface: "tui".to_owned(),
        cwd: None,
    };
    assert_eq!(
        loaded.register_tools(&mut registry, &call, &calls),
        vec!["camofox"]
    );
    Loaded {
        server,
        registry,
        _runtime: runtime,
        _profile: profile,
    }
}

async fn run(registry: &ToolRegistry, session: &str, name: &str, args: Value) -> ToolResult {
    let tool = registry.get(name).unwrap();
    let Executor::Handler(handler) = &tool.executor else {
        panic!("{name} runs through a handler");
    };
    let context = CallContext {
        call_id: "call_1".to_owned(),
        session_id: session.to_owned(),
        cancel: CancelToken::new(),
    };
    handler.call(&context, args).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_opens_reads_and_acts_on_its_own_tab() {
    let camofox = load();
    let registry = &camofox.registry;
    assert_eq!(
        registry.members("camofox").unwrap(),
        vec![
            "camofox_back",
            "camofox_click",
            "camofox_close",
            "camofox_open",
            "camofox_scroll",
            "camofox_snapshot",
            "camofox_type"
        ]
    );
    let page = run(
        registry,
        "ses_a",
        "camofox_open",
        json!({"url": "https://example.org/"}),
    )
    .await;
    assert_eq!(page.status, Status::Ok, "{}", page.text_content());
    assert!(
        page.text_content().contains("link \"More\" [e1]"),
        "{}",
        page.text_content()
    );
    let clicked = run(registry, "ses_a", "camofox_click", json!({"ref": "e1"})).await;
    assert_eq!(clicked.status, Status::Ok, "{}", clicked.text_content());
    run(registry, "ses_a", "camofox_close", json!({})).await;
    let requests = camofox.server.requests.lock().unwrap().clone();
    assert!(requests[0].starts_with("POST /tabs "), "{requests:?}");
    assert!(requests[0].contains("\"userId\":\"ses_a\""), "{requests:?}");
    assert!(
        requests
            .iter()
            .any(|r| r.starts_with("POST /tabs/tab-1/click ") && r.contains("\"ref\":\"e1\"")),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.starts_with("DELETE /tabs/tab-1?userId=ses_a")),
        "{requests:?}"
    );
    // Another session has no tab of its own yet.
    let other = run(registry, "ses_b", "camofox_snapshot", json!({})).await;
    assert_eq!(other.status, Status::Error);
    assert!(
        other.text_content().contains("no Camofox tab is open"),
        "{}",
        other.text_content()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_web_pages_open() {
    let camofox = load();
    for url in ["file:///etc/passwd", "javascript:alert(1)", "about:config"] {
        let refused = run(
            &camofox.registry,
            "ses_a",
            "camofox_open",
            json!({"url": url}),
        )
        .await;
        assert_eq!(refused.status, Status::Error, "{url}");
        assert!(
            refused.text_content().contains("only http(s) URLs"),
            "{}",
            refused.text_content()
        );
    }
    assert!(camofox.server.requests.lock().unwrap().is_empty());
}
