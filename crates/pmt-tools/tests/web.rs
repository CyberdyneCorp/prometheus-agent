//! The `web` toolset against mock backends (spec: tool-system, Single
//! purpose toolsets, Exposure requires toolset membership and capability
//! coverage; web-and-media-tools; security-boundaries, In-process fetch
//! SSRF protection). `public.test` resolves to a public address through a
//! fixed resolver; the backend mocks run on loopback.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use pmt_config::schema::{Config, WebConfig};
use pmt_core::capability::{BASELINE, CapabilitySet, Places};
use pmt_core::secret::SecretString;
use pmt_net::resolve::{Lookup, LookupFuture};
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_tools::environments::{Environments, SessionEnv, tool_output_reads};
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend};
use pmt_tools::result::Status;
use pmt_tools::web::{FullTextStore, SecretLookup, WebSession};
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
    session_in(config, &store_dir())
}

fn store_dir() -> std::path::PathBuf {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep()
}

fn session_in(config: &WebConfig, profile: &std::path::Path) -> WebSession {
    let client = pmt_net::client(&OutboundPolicy::default()).unwrap();
    WebSession::from_config(
        config,
        &client,
        fetcher(),
        &secrets(),
        FullTextStore::for_profile(profile),
    )
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

/// 60,000 characters of Markdown in 2,000 numbered lines.
fn long_page() -> String {
    (1..=2_000).fold(String::new(), |mut page, n| {
        let _ = writeln!(page, "paragraph {n:05} of a long page.");
        page
    })
}

#[tokio::test]
async fn a_long_page_is_windowed_deterministically_over_a_stored_full_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/scrape"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "data": {"markdown": long_page(), "metadata": {"title": "Long"}}
        })))
        .mount(&server)
        .await;
    let profile = store_dir();
    let web = session_in(&firecrawl_config(&server), &profile);
    let url = vec!["https://public.test/long".to_owned()];
    let first = web.extract(&url, None).await;
    let second = web.extract(&url, None).await;
    assert_eq!(first, second);
    let reply = body(&first);
    let content = reply["results"][0]["content"].as_str().unwrap();
    let chars = content.chars().count();
    assert!((14_000..=15_000).contains(&chars), "{chars}");
    assert!(content.starts_with("paragraph 00001"));
    let footer = &content[content.rfind("[TRUNCATED]").unwrap()..];
    let stored = profile.join("cache").join("web");
    let file = std::fs::read_dir(&stored)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(footer.contains(&file.display().to_string()), "{footer}");
    assert!(footer.contains("read_file(path="), "{footer}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), long_page());
    // A page within budget is returned whole.
    let whole = body(&web.extract(&url, Some(100_000)).await);
    assert_eq!(whole["results"][0]["content"], long_page());
}

#[tokio::test]
async fn auto_selects_a_backend_from_the_secrets_store() {
    let server = firecrawl().await;
    let mut config = WebConfig::default();
    // `auto` with FIRECRAWL_API_KEY present picks firecrawl (self-hosted
    // URL here only to reach the mock).
    config.firecrawl.base_url = Some(server.uri());
    let reply = body(&session(&config).search("rust", 1).await);
    assert_eq!(reply["backend"], "firecrawl");
}

/// A file session as session start builds it: the baseline plus
/// [`tool_output_reads`] (or the baseline alone), over a workspace in
/// `profile`.
fn file_session(profile: &Path, session_id: &str, with_tool_output: bool) -> FileSession {
    let work = profile.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut capabilities = CapabilitySet::parse(BASELINE).unwrap();
    if with_tool_output {
        capabilities = capabilities.union(&tool_output_reads(profile, session_id));
    }
    let places = Places {
        workspace: work.clone(),
        ..Places::default()
    };
    FileSession::new(FileSetup {
        backend: Arc::new(LocalBackend::new(capabilities, places)),
        cwd: Arc::new(move || work.clone()),
        user_home: None,
        control: ControlPaths {
            prometheus_home: profile.join("no-home"),
        },
        read_max_chars: 100_000,
        linter: Linter::builtin(),
    })
}

/// The `read_file` call a `[TRUNCATED]` footer names, as JSON arguments.
fn footer_call(content: &str) -> Value {
    let call = &content[content.rfind("read_file(").unwrap()..];
    let field = |name: &str| {
        let start = call.find(&format!("{name}=")).unwrap() + name.len() + 1;
        let rest = &call[start..];
        &rest[..rest.find([',', ')']).unwrap()]
    };
    json!({
        "path": field("path").trim_matches('"'),
        "offset": field("offset").parse::<u64>().unwrap(),
        "limit": field("limit").parse::<u64>().unwrap(),
    })
}

/// Spec scenario: the `[TRUNCATED]` footer's `read_file` call pages
/// through the omitted middle, through the dispatcher, with the
/// capabilities a session has.
#[tokio::test]
async fn the_footer_read_file_call_returns_the_omitted_middle() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/scrape"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "data": {"markdown": long_page(), "metadata": {"title": "Long"}}
        })))
        .mount(&server)
        .await;
    let profile = store_dir();
    let config = Config {
        web: firecrawl_config(&server),
        ..Config::default()
    };
    let environments = Arc::new(Environments::default());
    let attach = |with_tool_output: bool| {
        environments.attach(
            "ses_footer",
            SessionEnv {
                web: Some(Arc::new(session_in(&config.web, &profile))),
                files: Some(Arc::new(file_session(
                    &profile,
                    "ses_footer",
                    with_tool_output,
                ))),
                ..SessionEnv::default()
            },
        )
    };
    let attached = attach(true);
    let dispatcher =
        dispatcher::dispatcher(&environments, "ses_footer", &config, &[], &profile).await;
    let extract = dispatcher::call(
        &dispatcher,
        "web_extract",
        json!({"urls": ["https://public.test/long"]}),
    )
    .await;
    assert_eq!(extract.status, Status::Ok, "{}", extract.text_content());
    let content = body(&extract)["results"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let call = footer_call(&content);

    let read = dispatcher::call(&dispatcher, "read_file", call.clone()).await;
    assert_eq!(read.status, Status::Ok, "{}", read.text_content());
    let page = body(&read)["content"].as_str().unwrap().to_owned();
    let middle: Vec<&str> = page
        .lines()
        .map(|line| line.split_once('|').unwrap().1)
        .collect();
    let (offset, limit) = (
        usize::try_from(call["offset"].as_u64().unwrap()).unwrap(),
        usize::try_from(call["limit"].as_u64().unwrap()).unwrap(),
    );
    let expected: Vec<String> = (offset..offset + limit)
        .map(|n| format!("paragraph {n:05} of a long page."))
        .collect();
    assert_eq!(middle, expected);
    // Head, the read middle and the tail are the whole page.
    let head_last = format!("paragraph {:05} ", offset - 1);
    let tail_first = format!("paragraph {:05} ", offset + limit);
    assert!(content.contains(&head_last) && content.contains(&tail_first));

    // The baseline alone does not reach the profile cache.
    drop(attached);
    let _attached = attach(false);
    let refused = dispatcher::call(&dispatcher, "read_file", call).await;
    assert_eq!(refused.status, Status::Denied, "{}", refused.text_content());
    assert!(
        refused.text_content().contains("capability not granted"),
        "{}",
        refused.text_content()
    );
}

/// A spilled result's `<persisted-output>` path is readable with
/// `read_file` in the session that produced it.
#[tokio::test]
async fn a_spilled_result_is_readable_with_read_file() {
    let server = firecrawl().await;
    let profile = store_dir();
    let mut config = Config {
        web: firecrawl_config(&server),
        ..Config::default()
    };
    config
        .tools
        .result_budget
        .overrides
        .insert("web_search".into(), 500);
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_spill",
        SessionEnv {
            web: Some(Arc::new(session_in(&config.web, &profile))),
            files: Some(Arc::new(file_session(&profile, "ses_spill", true))),
            ..SessionEnv::default()
        },
    );
    let dispatcher =
        dispatcher::dispatcher(&environments, "ses_spill", &config, &[], &profile).await;
    let search = dispatcher::call(
        &dispatcher,
        "web_search",
        json!({"query": "rust", "limit": 20}),
    )
    .await;
    let text = search.text_content();
    let marker = "Full output saved to: ";
    let start = text.find(marker).expect(&text) + marker.len();
    let spilled = &text[start..start + text[start..].find('\n').unwrap()];
    let read = dispatcher::call(&dispatcher, "read_file", json!({"path": spilled})).await;
    assert_eq!(read.status, Status::Ok, "{}", read.text_content());
    assert!(
        body(&read)["content"]
            .as_str()
            .unwrap()
            .contains("Result 20")
    );
}
