//! The `web` toolset against mock backends (spec: tool-system, Single
//! purpose toolsets, Exposure requires toolset membership and capability
//! coverage; web-and-media-tools; security-boundaries, In-process fetch
//! SSRF protection). `public.test` resolves to a public address through a
//! fixed resolver; the backend mocks run on loopback.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use pmt_config::schema::{Config, WebConfig};
use pmt_core::secret::SecretString;
use pmt_net::resolve::{Lookup, LookupFuture};
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::result::Status;
use pmt_tools::web::{SecretLookup, WebSession};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "common/dispatcher.rs"]
mod dispatcher;

struct Names(HashMap<&'static str, Vec<IpAddr>>);

impl Lookup for Names {
    fn lookup(&self, host: &str) -> LookupFuture {
        let answer = self
            .0
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, host.to_owned()));
        Box::pin(async move { answer })
    }
}

fn fetcher() -> Arc<Fetcher> {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    let names = Arc::new(Names(HashMap::from([
        ("public.test", vec![ip("93.184.216.34")]),
        ("metadata.test", vec![ip("169.254.169.254")]),
        ("intranet.test", vec![ip("10.1.2.3")]),
    ])));
    let policy = DestinationPolicy::new(false, &[]).unwrap();
    Arc::new(Fetcher::with_lookup(&OutboundPolicy::default(), policy, names).unwrap())
}

fn secrets() -> SecretLookup {
    Arc::new(|name: &str| (name == "FIRECRAWL_API_KEY").then(|| SecretString::new("fc-test-key")))
}

fn session(config: &WebConfig) -> WebSession {
    let client = pmt_net::client(&OutboundPolicy::default()).unwrap();
    WebSession::from_config(config, &client, fetcher(), &secrets())
}

fn body(result: &pmt_tools::ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("not JSON: {}", result.text_content()))
}

async fn firecrawl() -> MockServer {
    let server = MockServer::start().await;
    let hits: Vec<Value> = (1..=20)
        .map(|n| json!({"url": format!("https://r{n}.example/"), "title": format!("Result {n}"), "description": "about rust"}))
        .collect();
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .and(header("authorization", "Bearer fc-test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"success": true, "data": hits})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/scrape"))
        .and(body_partial_json(
            json!({"url": "https://public.test/slow"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(5))
                .set_body_json(json!({"success": true, "data": {"markdown": "late"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/scrape"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "data": {"markdown": "# Docs\n![logo](https://public.test/logo.png)\nBody text.", "metadata": {"title": "Docs"}}
        })))
        .mount(&server)
        .await;
    server
}

fn firecrawl_config(server: &MockServer) -> WebConfig {
    let mut config = WebConfig {
        backend: "firecrawl".into(),
        ..WebConfig::default()
    };
    config.firecrawl.base_url = Some(server.uri());
    config
}

#[tokio::test]
async fn search_goes_to_the_configured_backend_and_the_limit_bounds_the_list() {
    let server = firecrawl().await;
    let result = session(&firecrawl_config(&server))
        .search("rust async", 3)
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let reply = body(&result);
    assert_eq!(reply["backend"], "firecrawl");
    let results = reply["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let positions: Vec<&Value> = results.iter().map(|r| &r["position"]).collect();
    assert_eq!(positions, [&json!(1), &json!(2), &json!(3)]);
    assert_eq!(results[0]["snippet"], "about rust");
}

#[tokio::test]
async fn a_backend_outage_is_an_error_naming_the_backend() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mut config = WebConfig {
        backend: "searxng".into(),
        ..WebConfig::default()
    };
    config.searxng.url = Some(server.uri());
    let result = session(&config).search("x", 5).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "web_search failed (searxng): HTTP 503"
    );
}

#[tokio::test]
async fn each_capability_uses_its_own_backend() {
    let crawl = firecrawl().await;
    let searx = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "site:docs.rs tokio"))
        .and(query_param("format", "json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"url": "https://docs.rs/tokio", "title": "tokio", "content": "async runtime"}]
        })))
        .mount(&searx)
        .await;
    let mut config = firecrawl_config(&crawl);
    config.backend = "auto".into();
    config.search_backend = Some("searxng".into());
    config.extract_backend = Some("firecrawl".into());
    config.searxng.url = Some(searx.uri());
    let web = session(&config);
    let search = body(&web.search("site:docs.rs tokio", 5).await);
    assert_eq!(search["backend"], "searxng");
    assert_eq!(search["results"][0]["url"], "https://docs.rs/tokio");
    let extract = body(
        &web.extract(&["https://public.test/docs".into()], None)
            .await,
    );
    assert_eq!(extract["backend"], "firecrawl");
    assert_eq!(extract["results"][0]["status"], "ok");
    assert_eq!(extract["results"][0]["title"], "Docs");
    assert_eq!(
        extract["results"][0]["content"],
        "# Docs\n[IMAGE: logo]\nBody text."
    );
}

#[tokio::test]
async fn private_metadata_and_unresolvable_urls_are_refused_before_the_backend() {
    let server = firecrawl().await;
    let web = session(&firecrawl_config(&server));
    let urls: Vec<String> = [
        "http://169.254.169.254/latest/meta-data",
        "http://metadata.test/",
        "http://intranet.test/wiki",
        "http://127.0.0.1:8080/admin",
        "https://does-not-resolve.test/",
        "file:///etc/passwd",
    ]
    .map(str::to_owned)
    .into();
    let result = web.extract(&urls[..5], None).await;
    assert_eq!(result.status, Status::Error, "{}", result.text_content());
    for entry in body(&result)["results"].as_array().unwrap() {
        assert_eq!(entry["status"], "blocked", "{entry}");
    }
    let file = body(&web.extract(&urls[5..], None).await);
    assert_eq!(file["results"][0]["status"], "blocked");
    // None of them reached the backend.
    let scrapes = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/v1/scrape")
        .count();
    assert_eq!(scrapes, 0);
}

#[tokio::test]
async fn one_failing_url_does_not_fail_the_others() {
    let server = firecrawl().await;
    let mut config = firecrawl_config(&server);
    config.extract_timeout_secs = 1;
    let urls = [
        "https://public.test/docs".to_owned(),
        "https://public.test/slow".to_owned(),
    ];
    let result = session(&config).extract(&urls, None).await;
    assert_eq!(result.status, Status::Ok);
    let reply = body(&result);
    assert_eq!(reply["results"][0]["status"], "ok");
    assert_eq!(reply["results"][1]["status"], "timeout");
}

#[tokio::test]
async fn the_tools_are_exposed_only_with_a_configured_backend() {
    let environments = Arc::new(Environments::default());
    let profile = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut config = Config::default();
    let dispatcher =
        dispatcher::dispatcher(&environments, "ses_w", &config, &[], profile.path()).await;
    let unexposed = dispatcher::call(&dispatcher, "web_search", json!({"query": "x"})).await;
    assert_eq!(unexposed.text_content(), "unknown tool: web_search");

    let server = firecrawl().await;
    config.web = firecrawl_config(&server);
    let _attached = environments.attach(
        "ses_w",
        SessionEnv {
            web: Some(Arc::new(session(&config.web))),
            ..SessionEnv::default()
        },
    );
    let dispatcher =
        dispatcher::dispatcher(&environments, "ses_w", &config, &[], profile.path()).await;
    let found = dispatcher::call(
        &dispatcher,
        "web_search",
        json!({"query": "rust", "limit": 2}),
    )
    .await;
    assert_eq!(found.status, Status::Ok, "{}", found.text_content());
    assert_eq!(body(&found)["results"].as_array().unwrap().len(), 2);
}
