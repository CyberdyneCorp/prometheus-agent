//! First-party cloud browser providers against mock provider endpoints,
//! with a local browser standing in for the provider's remote one (spec:
//! execution-environments, First-party cloud browser providers; Private
//! URLs route to a local browser; Browser session resolution).

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_core::secret::SecretString;
use pmt_tools::browser::cloud::{
    BrowserUse, Browserbase, CloudProvider, Endpoint, Firecrawl, Subscription, TokenSource,
};
use pmt_tools::browser::connect::Status;
use pmt_tools::browser::status::Sessions;
use pmt_tools::browser::{BrowserManager, Origin};
use pmt_tools::result::Status as ResultStatus;
use pmt_tools::web::SecretLookup;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{self, External, Fixture, run, site};

fn secrets(pairs: &[(&str, String)]) -> SecretLookup {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    Arc::new(move |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| SecretString::new(v))
    })
}

fn client() -> pmt_tools::browser::cloud::HttpClient {
    Arc::new(pmt_net::LazyClient::ready(
        pmt_net::client_builder(&pmt_net::OutboundPolicy::default())
            .unwrap()
            .no_proxy()
            .build()
            .unwrap(),
    ))
}

/// A mock Browserbase whose sessions are `browser`'s.
async fn browserbase(browser: &External) -> MockServer {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions"))
        .and(header("X-BB-API-Key", "bb-key"))
        .and(body_partial_json(json!({"projectId": "proj"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id": "bb-1", "connectUrl": browser.endpoint})),
        )
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions/bb-1"))
        .and(body_partial_json(json!({"status": "REQUEST_RELEASE"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&api)
        .await;
    api
}

fn browserbase_secrets(api: &MockServer) -> SecretLookup {
    secrets(&[
        ("BROWSERBASE_API_KEY", "bb-key".to_owned()),
        ("BROWSERBASE_PROJECT_ID", "proj".to_owned()),
        ("BROWSERBASE_BASE_URL", api.uri()),
    ])
}

fn manager_with(
    fixture: &Fixture,
    name: &str,
    provider: Arc<dyn CloudProvider>,
) -> Arc<BrowserManager> {
    let providers: BTreeMap<String, Arc<dyn CloudProvider>> =
        BTreeMap::from([(name.to_owned(), provider)]);
    fixture.manager_with(Fixture::chromium(), providers)
}

async fn requests_to(api: &MockServer, wanted: &str) -> usize {
    api.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == wanted)
        .count()
}

/// Scenario: Key present, not configured.
#[tokio::test]
async fn a_provider_key_alone_never_selects_the_provider() {
    let fixture = Fixture::new();
    let api = MockServer::start().await;
    let provider = Arc::new(Browserbase::new(client(), browserbase_secrets(&api)));
    // `browser.provider` stays at its default.
    assert_eq!(fixture.config.lock().unwrap().provider, "local");
    let manager = manager_with(&fixture, "browserbase", provider);
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Local);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
    drop(session);
    manager.shutdown().await;
}

#[tokio::test]
async fn a_browserbase_session_is_created_reported_and_released() {
    let fixture = Fixture::new();
    let executable = support::browsers()[0].clone();
    let remote = External::start(&executable, &fixture.home.join("remote"));
    let api = browserbase(&remote).await;
    fixture.config.lock().unwrap().provider = "browserbase".to_owned();
    let provider = Arc::new(Browserbase::new(client(), browserbase_secrets(&api)));
    let manager = manager_with(&fixture, "browserbase", provider);
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, ResultStatus::Ok, "{body}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Cloud("browserbase".to_owned()));
    assert_eq!(session.provider(), Some("browserbase"));
    drop(session);
    let live = Sessions::new(&fixture.browser_cache()).live();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].provider.as_deref(), Some("browserbase"));
    assert_eq!(live[0].endpoint, None, "a provider URL may carry a key");
    manager.shutdown().await;
    assert_eq!(requests_to(&api, "/v1/sessions/bb-1").await, 1, "released");
    assert!(Sessions::new(&fixture.browser_cache()).live().is_empty());
    remote.stop();
}

#[tokio::test]
async fn a_plan_without_keep_alive_or_proxies_is_retried_without_them() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions"))
        .and(body_partial_json(json!({"keepAlive": true})))
        .respond_with(ResponseTemplate::new(402))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions"))
        .and(body_partial_json(json!({"proxies": true})))
        .respond_with(ResponseTemplate::new(402))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"id": "bb-2", "connectUrl": format!("{}/connect", api.uri().replace("http", "ws"))}),
        ))
        .mount(&api)
        .await;
    let provider = Browserbase::new(client(), browserbase_secrets(&api));
    let created = provider.create().await.unwrap();
    assert_eq!(created.id, "bb-2");
    assert_eq!(api.received_requests().await.unwrap().len(), 3);
}

/// The providers a session start wires build no HTTP client until one of
/// them sends a request (a client build loads the system's root
/// certificates, which the startup budget cannot afford per provider).
#[tokio::test]
async fn the_providers_build_their_client_on_the_first_request() {
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&builds);
    let lazy: pmt_tools::browser::cloud::HttpClient =
        Arc::new(pmt_net::LazyClient::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            pmt_net::client_builder(&pmt_net::OutboundPolicy::default())?
                .no_proxy()
                .build()
                .map_err(|e| {
                    pmt_core::error::Error::new(
                        &pmt_core::error::codes::NET_CLIENT_INIT,
                        e.to_string(),
                    )
                })
        }));
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"id": "bb-1", "connectUrl": format!("{}/connect", api.uri().replace("http", "ws"))}),
        ))
        .mount(&api)
        .await;
    let providers = pmt_tools::browser::cloud::first_party(
        &lazy,
        &browserbase_secrets(&api),
        Arc::new(|| None),
    );
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 0);
    providers["browserbase"].create().await.unwrap();
    providers["browserbase"].create().await.unwrap();
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_keys_and_undeclared_hosts_are_refused() {
    let none = Browserbase::new(client(), secrets(&[]));
    let refused = none.create().await.unwrap_err();
    assert!(refused.contains("BROWSERBASE_API_KEY"), "{refused}");
    // The vendor's API answering with a DevTools URL on another host.
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/browser"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "f-1", "cdpUrl": "wss://evil.example/devtools"})),
        )
        .mount(&api)
        .await;
    let firecrawl = Firecrawl::new(
        client(),
        secrets(&[
            ("FIRECRAWL_API_KEY", "fc".to_owned()),
            ("FIRECRAWL_API_URL", api.uri()),
        ]),
    );
    let refused = firecrawl.create().await.unwrap_err();
    assert!(refused.contains("evil.example"), "{refused}");
}

#[tokio::test]
async fn browser_use_firecrawl_and_the_subscription_create_and_release() {
    let api = MockServer::start().await;
    let ws = format!("{}/devtools/browser/x", api.uri().replace("http", "ws"));
    Mock::given(method("POST"))
        .and(path("/browsers"))
        .and(header("X-Browser-Use-API-Key", "bu-key"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "bu-1", "cdpUrl": ws})))
        .mount(&api)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/browsers/bu-1"))
        .and(body_partial_json(json!({"action": "stop"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&api)
        .await;
    let custom = Endpoint::of("https://unused.invalid", Some(SecretString::new(api.uri())));
    let browser_use = BrowserUse::at(
        client(),
        secrets(&[("BROWSER_USE_API_KEY", "bu-key".to_owned())]),
        custom.clone(),
    );
    let created = browser_use.create().await.unwrap();
    assert_eq!(
        (created.id.as_str(), created.endpoint.as_str()),
        ("bu-1", ws.as_str())
    );
    browser_use.release("bu-1").await.unwrap();

    // The subscription: the same API through the gateway, billed to the
    // signed-in subscription (its token as the key), with an idempotency key.
    let token: TokenSource = Arc::new(|| Some(SecretString::new("bu-key")));
    let subscription = Subscription::at(client(), token, custom.clone());
    assert_eq!(subscription.create().await.unwrap().id, "bu-1");
    let seen = api.received_requests().await.unwrap();
    let last = seen
        .iter()
        .rev()
        .find(|r| r.url.path() == "/browsers")
        .unwrap();
    assert!(last.headers.get("X-Idempotency-Key").is_some());
    let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(body["timeout"], 5);
    let signed_out: TokenSource = Arc::new(|| None);
    let refused = Subscription::at(client(), signed_out, custom)
        .create()
        .await
        .unwrap_err();
    assert!(refused.contains("auth add nous"), "{refused}");

    Mock::given(method("POST"))
        .and(path("/v2/browser"))
        .and(header("Authorization", "Bearer fc-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "fc-1", "cdpUrl": ws})))
        .mount(&api)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/browser/fc-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&api)
        .await;
    let firecrawl = Firecrawl::new(
        client(),
        secrets(&[
            ("FIRECRAWL_API_KEY", "fc-key".to_owned()),
            ("FIRECRAWL_API_URL", api.uri()),
        ]),
    );
    assert_eq!(firecrawl.create().await.unwrap().id, "fc-1");
    firecrawl.release("fc-1").await.unwrap();
}

/// Scenario: Localhost under Browserbase.
#[tokio::test]
async fn a_private_url_opens_in_a_local_sidecar_and_never_reaches_the_provider() {
    let fixture = Fixture::new();
    let executable = support::browsers()[0].clone();
    let remote = External::start(&executable, &fixture.home.join("remote"));
    let api = browserbase(&remote).await;
    fixture.config.lock().unwrap().provider = "browserbase".to_owned();
    let provider = Arc::new(Browserbase::new(client(), browserbase_secrets(&api)));
    let manager = manager_with(&fixture, "browserbase", provider);
    let exec = fixture.exec(&manager, false);
    let page = site().await;
    // A loopback address (the mock site listens on 127.0.0.1 only).
    let code = format!(
        "new_tab('{}'); wait_for_load(); print(page_info()['title'])",
        page.uri()
    );
    let (result, body) = run(&exec, &code).await;
    assert_eq!(result.status, ResultStatus::Ok, "{body}");
    assert_eq!(body["output"], "Test page\n");
    assert_eq!(body["local_sidecar"], true);
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Cloud("browserbase".to_owned()));
    let sidecar = session.sidecar().expect("a sidecar");
    assert_eq!(sidecar.origin(), &Origin::Local);
    drop((session, sidecar));
    let port = page.address().port().to_string();
    let seen = Status::read(&remote.http()).await.unwrap();
    assert!(
        seen.targets.iter().all(|(_, _, url)| !url.contains(&port)),
        "the provider's browser saw the private URL: {:?}",
        seen.targets
    );
    let api_paths: Vec<String> = api
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(api_paths, ["/v1/sessions"], "only the session was created");

    // Cloud metadata endpoints are refused on every route.
    let (refused, body) = run(&exec, "goto_url('http://169.254.169.254/latest/meta-data')").await;
    assert_eq!(refused.status, ResultStatus::Error);
    assert!(body.to_string().contains("metadata"), "{body}");
    manager.shutdown().await;
    remote.stop();
}

#[tokio::test]
async fn with_the_switch_off_a_private_url_is_refused() {
    let mut fixture = Fixture::new();
    fixture.allow_private = false;
    let executable = support::browsers()[0].clone();
    let remote = External::start(&executable, &fixture.home.join("remote"));
    let api = browserbase(&remote).await;
    {
        let mut config = fixture.config.lock().unwrap();
        config.provider = "browserbase".to_owned();
        config.auto_local_for_private_urls = false;
    }
    let provider = Arc::new(Browserbase::new(client(), browserbase_secrets(&api)));
    let manager = manager_with(&fixture, "browserbase", provider);
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "new_tab('http://localhost:3000/')").await;
    assert_eq!(result.status, ResultStatus::Error);
    assert!(
        body.to_string()
            .contains("Blocked: URL targets a private or internal address"),
        "{body}"
    );
    let session = manager.session("ses_b/default").await.unwrap();
    assert!(session.sidecar().is_none());
    drop(session);
    manager.shutdown().await;
    remote.stop();
}
