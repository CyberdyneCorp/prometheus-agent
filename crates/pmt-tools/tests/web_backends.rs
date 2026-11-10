//! The M2 web backends against mock vendors (spec: web-and-media-tools,
//! Built-in web backends with capability flags; Keyless web search
//! rotation is explicit opt-in; Web result caching; Media tool credentials
//! and billing are explicit). Vendor APIs run on loopback mocks reached
//! through the session's endpoint overrides; `public.test` resolves to a
//! public address through a fixed resolver.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::{Config, WebConfig, WireFormat};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::secret::SecretString;
use pmt_net::resolve::{Lookup, LookupFuture};
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_tools::probe::ProbeCache;
use pmt_tools::result::Status;
use pmt_tools::surface::Exclusion;
use pmt_tools::usage::{BILLING_SOURCE, CACHED, ToolUsage};
use pmt_tools::variants::RouteTraits;
use pmt_tools::web::{FullTextStore, GatewayRoute, SecretLookup, WebDeps, WebSession};
use pmt_tools::{Builtins, Selection, SurfaceInputs, ToolRegistry, ToolResult, ToolSurface};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Names;

impl Lookup for Names {
    fn lookup(&self, host: &str) -> LookupFuture {
        let tld = host.rsplit('.').next().unwrap_or_default();
        let answer = if matches!(tld, "test" | "app") {
            Ok(vec!["93.184.216.34".parse::<IpAddr>().unwrap()])
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, host.to_owned()))
        };
        Box::pin(async move { answer })
    }
}

fn fetcher() -> Arc<Fetcher> {
    let policy = DestinationPolicy::new(false, &[]).unwrap();
    Arc::new(Fetcher::with_lookup(&OutboundPolicy::default(), policy, Arc::new(Names)).unwrap())
}

fn profile() -> std::path::PathBuf {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep()
}

fn keys(names: &[&'static str]) -> SecretLookup {
    let names = names.to_vec();
    Arc::new(move |name: &str| {
        names
            .contains(&name)
            .then(|| SecretString::new(format!("{name}-value")))
    })
}

struct Setup {
    config: WebConfig,
    secrets: SecretLookup,
    endpoints: BTreeMap<String, String>,
    gateway: Option<GatewayRoute>,
    profile: std::path::PathBuf,
}

impl Setup {
    fn new(config: WebConfig) -> Self {
        Self {
            config,
            secrets: keys(&[]),
            endpoints: BTreeMap::new(),
            gateway: None,
            profile: profile(),
        }
    }

    fn at(mut self, id: &str, server: &MockServer) -> Self {
        self.endpoints.insert(id.to_owned(), server.uri());
        self
    }

    fn session(&self) -> WebSession {
        let client = pmt_net::client(&OutboundPolicy::default()).unwrap();
        let mut deps = WebDeps::new(
            &client,
            fetcher(),
            &self.secrets,
            FullTextStore::for_profile(&self.profile),
        );
        deps.endpoints = self.endpoints.clone();
        deps.gateway.clone_from(&self.gateway);
        WebSession::build(&self.config, deps)
    }
}

fn body(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("not JSON: {}", result.text_content()))
}

fn named(backend: &str) -> WebConfig {
    WebConfig {
        backend: backend.into(),
        ..WebConfig::default()
    }
}

async fn surface(config: &Config, route: &RouteTraits) -> ToolSurface {
    let environments = Arc::new(pmt_tools::environments::Environments::default());
    let tools = pmt_tools::builtin_tools(&environments);
    let registry = ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()));
    let selection = Selection::resolve(&registry, &["web".to_owned()], &[]).unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
        route,
    })
    .await
}

#[tokio::test]
async fn a_search_only_backend_leaves_extract_unavailable() {
    let mut config = Config {
        web: named("searxng"),
        ..Config::default()
    };
    config.web.searxng.url = Some("http://127.0.0.1:9".into());
    let surface = surface(&config, &RouteTraits::default()).await;
    assert!(surface.get("web_search").is_some());
    assert_eq!(
        surface.excluded()["web_extract"].to_string(),
        "unavailable:backend 'searxng' is search-only; set web.extract_backend"
    );
}

#[tokio::test]
async fn openai_native_needs_the_codex_responses_route_and_nothing_replaces_it() {
    let mut config = Config::default();
    config.web.search_backend = Some("openai-native".into());
    let anthropic = RouteTraits::of(
        "anthropic",
        "claude-sonnet-4",
        WireFormat::AnthropicMessages,
    );
    let off_route = surface(&config, &anthropic).await;
    assert!(off_route.get("web_search").is_none());
    let Exclusion::Unavailable(reason) = &off_route.excluded()["web_search"] else {
        panic!("{:?}", off_route.excluded());
    };
    assert!(reason.contains("openai-codex Responses route"), "{reason}");
    // On its route the definition is the provider's own search tool.
    let codex = RouteTraits::of("openai-codex", "gpt-5-codex", WireFormat::Responses);
    let on_route = surface(&config, &codex).await;
    let definition = on_route
        .definitions()
        .iter()
        .find(|d| d.name == "web_search")
        .unwrap();
    assert_eq!(definition.parameters["x-provider-executed"], "web_search");
}

#[tokio::test]
async fn a_default_install_never_reaches_a_keyless_vendor() {
    let vendor = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .expect(0)
        .mount(&vendor)
        .await;
    let mut setup = Setup::new(WebConfig::default());
    for id in ["exa", "parallel", "firecrawl", "keenable", "ddgs"] {
        setup = setup.at(id, &vendor);
    }
    let result = setup.session().search("rust", 3).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "web_search unavailable: no web backend configured"
    );
    let surface = surface(&Config::default(), &RouteTraits::default()).await;
    assert_eq!(
        surface.excluded()["web_search"].to_string(),
        "unavailable:no web backend configured"
    );
}

async fn refusing_tavily() -> MockServer {
    let tavily = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("authorization", "Bearer TAVILY_API_KEY-value"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&tavily)
        .await;
    tavily
}

async fn free_exa() -> MockServer {
    let exa = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"title": "Free", "url": "https://free.test/", "highlights": ["from the free tier"]}
        ]})))
        .mount(&exa)
        .await;
    exa
}

#[tokio::test]
async fn opt_in_rescue_answers_once_per_call_and_never_skips_the_keyed_backend() {
    let tavily = refusing_tavily().await;
    let exa = free_exa().await;
    let limited = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&limited)
        .await;
    let mut config = named("tavily");
    config.keyless.enabled = true;
    config.keyless.rescue = true;
    let mut setup = Setup::new(config).at("tavily", &tavily).at("exa", &exa);
    for id in ["parallel", "firecrawl", "keenable"] {
        setup = setup.at(id, &limited);
    }
    setup.secrets = keys(&["TAVILY_API_KEY"]);
    let web = setup.session();
    for _ in 0..2 {
        let result = web.search("rust async", 3).await;
        assert_eq!(result.status, Status::Ok, "{}", result.text_content());
        let reply = body(&result);
        assert_eq!(reply["backend"], "keyless:exa");
        assert_eq!(reply["rescued"], true);
        assert_eq!(result.metadata[BILLING_SOURCE], "keyless:exa");
    }
    // `.expect(2)` on Tavily: both calls tried it first.
}

#[tokio::test]
async fn without_rescue_a_refused_keyed_backend_is_an_error() {
    let tavily = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&tavily)
        .await;
    let mut setup = Setup::new(named("tavily")).at("tavily", &tavily);
    setup.secrets = keys(&["TAVILY_API_KEY"]);
    let result = setup.session().search("rust", 3).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "web_search failed (tavily): HTTP 401"
    );
}

#[tokio::test]
async fn the_keyless_ring_rotates_past_rate_limits() {
    let limited = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&limited)
        .await;
    let exa = free_exa().await;
    let setup = Setup::new(named("keyless"))
        .at("exa", &exa)
        .at("parallel", &limited)
        .at("firecrawl", &limited)
        .at("keenable", &limited);
    let web = setup.session();
    for query in ["a", "b", "c", "d"] {
        let result = web.search(query, 1).await;
        assert_eq!(body(&result)["backend"], "keyless:exa", "{query}");
    }
}

#[tokio::test]
async fn subagents_issuing_the_same_search_at_once_share_one_request() {
    let tavily = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(body_partial_json(json!({"max_results": 10})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_json(json!({"results": [
                    {"title": "One", "url": "https://one.test/", "content": "1"},
                    {"title": "Two", "url": "https://two.test/", "content": "2"}
                ]})),
        )
        .expect(1)
        .mount(&tavily)
        .await;
    let mut setup = Setup::new(named("tavily")).at("tavily", &tavily);
    setup.secrets = keys(&["TAVILY_API_KEY"]);
    let (a, b, c) = (setup.session(), setup.session(), setup.session());
    let (ra, rb, rc) = tokio::join!(
        a.search("Rust  Async", 2),
        b.search("rust async", 2),
        c.search(" rust ASYNC", 1)
    );
    for result in [&ra, &rb] {
        assert_eq!(body(result)["results"], body(&ra)["results"]);
    }
    assert_eq!(body(&rc)["results"].as_array().unwrap().len(), 1);
    let cached = [&ra, &rb, &rc]
        .iter()
        .filter(|r| r.metadata.get(CACHED) == Some(&Value::Bool(true)))
        .count();
    assert_eq!(cached, 2);
}

async fn extractor() -> MockServer {
    let tavily = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/extract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"url": "https://x/", "title": "Page", "raw_content": "# Page\nbody"}
        ]})))
        .mount(&tavily)
        .await;
    tavily
}

#[tokio::test]
async fn an_exempt_preview_host_is_extracted_live_every_time() {
    let tavily = extractor().await;
    let mut config = named("tavily");
    config.cache_exempt_hosts = vec!["*.ngrok-free.app".into()];
    let mut setup = Setup::new(config).at("tavily", &tavily);
    setup.secrets = keys(&["TAVILY_API_KEY"]);
    let web = setup.session();
    let preview = vec!["https://abc.ngrok-free.app/".to_owned()];
    for _ in 0..2 {
        let result = web.extract(&preview, None).await;
        assert_eq!(result.status, Status::Ok, "{}", result.text_content());
        assert!(result.metadata.get(CACHED).is_none());
    }
    let public = vec!["https://docs.test/".to_owned()];
    let first = web.extract(&public, None).await;
    let second = web.extract(&public, None).await;
    assert_eq!(first.text_content(), second.text_content());
    assert_eq!(second.metadata[CACHED], true);
    let requests = tavily.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "two preview calls, one public");
}

/// A mock answering `path` with `reply` when the request carries
/// `auth = value`.
async fn vendor(route: &str, auth: (&str, &str), reply: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path(route))
        .and(header(auth.0, auth.1))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn each_keyed_vendor_is_asked_and_read_in_its_own_shape() {
    let cases = [
        (
            "tavily",
            "/search",
            ("authorization", "Bearer TAVILY_API_KEY-value"),
            json!({"results": [{"title": "T", "url": "https://t.test/", "content": "tav"}]}),
            "tav",
        ),
        (
            "exa",
            "/search",
            ("x-api-key", "EXA_API_KEY-value"),
            json!({"results": [{"title": "E", "url": "https://e.test/", "highlights": ["exa"]}]}),
            "exa",
        ),
        (
            "parallel",
            "/v1beta/search",
            ("x-api-key", "PARALLEL_API_KEY-value"),
            json!({"results": [{"title": "P", "url": "https://p.test/", "excerpts": ["par"]}]}),
            "par",
        ),
        (
            "keenable",
            "/v1/search",
            ("authorization", "Bearer KEENABLE_API_KEY-value"),
            json!({"results": [{"title": "K", "url": "https://k.test/", "snippet": "kee"}]}),
            "kee",
        ),
        (
            "perplexity",
            "/search",
            ("authorization", "Bearer PERPLEXITY_API_KEY-value"),
            json!({"results": [{"title": "X", "url": "https://x.test/", "snippet": "ppx"}]}),
            "ppx",
        ),
        (
            "brave-free",
            "/res/v1/web/search",
            ("x-subscription-token", "BRAVE_SEARCH_API_KEY-value"),
            json!({"web": {"results": [{"title": "B", "url": "https://b.test/", "description": "brv"}]}}),
            "brv",
        ),
    ];
    for (id, route, auth, reply, snippet) in cases {
        let server = vendor(route, auth, reply).await;
        let mut setup = Setup::new(named(id)).at(id, &server);
        setup.secrets = keys(&[
            "TAVILY_API_KEY",
            "EXA_API_KEY",
            "PARALLEL_API_KEY",
            "KEENABLE_API_KEY",
            "PERPLEXITY_API_KEY",
            "BRAVE_SEARCH_API_KEY",
        ]);
        let result = setup.session().search("q", 5).await;
        assert_eq!(result.status, Status::Ok, "{id}: {}", result.text_content());
        let reply = body(&result);
        assert_eq!(reply["backend"], id);
        assert_eq!(reply["results"][0]["snippet"], snippet, "{id}");
        assert_eq!(result.metadata[BILLING_SOURCE], format!("api_key:{id}"));
    }
}

#[tokio::test]
async fn brave_free_searches_with_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "site:docs.rs tokio"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"web": {"results": []}})))
        .expect(1)
        .mount(&server)
        .await;
    let mut setup = Setup::new(named("brave-free")).at("brave-free", &server);
    setup.secrets = keys(&["BRAVE_SEARCH_API_KEY"]);
    let result = setup.session().search("site:docs.rs tokio", 5).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
}

#[tokio::test]
async fn ddgs_is_keyless_and_reads_html_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/html/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<a class="result__a" href="https://d.test/">Duck</a> <a class="result__snippet" href="x">quack</a>"#,
        ))
        .mount(&server)
        .await;
    let setup = Setup::new(named("ddgs")).at("ddgs", &server);
    let result = setup.session().search("duck", 5).await;
    let reply = body(&result);
    assert_eq!(reply["results"][0]["url"], "https://d.test/");
    assert_eq!(result.metadata[BILLING_SOURCE], "keyless:ddgs");
    let mut config = named("ddgs");
    config.extract_backend = None;
    let surface = surface(
        &Config {
            web: config,
            ..Config::default()
        },
        &RouteTraits::default(),
    )
    .await;
    assert!(
        surface.excluded()["web_extract"]
            .to_string()
            .contains("search-only")
    );
}

#[tokio::test]
async fn xai_search_reads_citations_and_applies_domain_filters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer XAI_API_KEY-value"))
        .and(body_partial_json(json!({"model": "grok-4.5", "tools": [
            {"type": "web_search", "filters": {"allowed_domains": ["docs.rs"]}}]})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"output": [{"content": [
            {"type": "output_text", "text": "x", "annotations": [
                {"type": "url_citation", "url": "https://docs.rs/tokio", "title": "tokio"}]}]}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut config = named("xai");
    config.xai.allowed_domains = vec!["docs.rs".into()];
    let mut setup = Setup::new(config).at("xai", &server);
    setup.secrets = keys(&["XAI_API_KEY"]);
    let result = setup.session().search("tokio", 5).await;
    assert_eq!(body(&result)["results"][0]["url"], "https://docs.rs/tokio");
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:xai");
}

#[tokio::test]
async fn nous_goes_through_the_tool_gateway_and_bills_the_subscription() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/web/search"))
        .and(header("authorization", "Bearer sub-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"title": "N", "url": "https://n.test/", "snippet": "via gateway"}]})))
        .mount(&gateway)
        .await;
    let mut setup = Setup::new(named("nous"));
    setup.gateway = Some(GatewayRoute {
        url: gateway.uri(),
        token: SecretString::new("sub-token"),
        billing_source: "subscription:nous/personal".into(),
    });
    let web = setup.session();
    let result = web.search("q", 5).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(
        body(&result)["billing_source"],
        "subscription:nous/personal"
    );
    // Without a sign-in nothing else is substituted.
    let unsigned = Setup::new(named("nous")).session().search("q", 5).await;
    assert!(unsigned.text_content().contains("Nous Portal sign-in"));
}

#[tokio::test]
async fn a_metered_search_is_recorded_once_and_its_cached_repeat_is_not() {
    let environments = Arc::new(pmt_tools::environments::Environments::default());
    let tavily = vendor(
        "/search",
        ("authorization", "Bearer TAVILY_API_KEY-value"),
        json!({"results": [{"title": "T", "url": "https://t.test/", "content": "c"}]}),
    )
    .await;
    let seen: Arc<Mutex<Vec<ToolUsage>>> = Arc::default();
    let into = Arc::clone(&seen);
    let mut setup = Setup::new(named("tavily")).at("tavily", &tavily);
    setup.secrets = keys(&["TAVILY_API_KEY"]);
    let client = pmt_net::client(&OutboundPolicy::default()).unwrap();
    let mut deps = WebDeps::new(
        &client,
        fetcher(),
        &setup.secrets,
        FullTextStore::for_profile(&setup.profile),
    );
    deps.endpoints = setup.endpoints.clone();
    deps.usage = Some(Arc::new(move |u| into.lock().unwrap().push(u)));
    let _attached = environments.attach(
        "ses_usage",
        pmt_tools::environments::SessionEnv {
            web: Some(Arc::new(WebSession::build(&setup.config, deps))),
            ..Default::default()
        },
    );
    let tools: HashMap<String, _> = pmt_tools::web::tools(&environments)
        .into_iter()
        .map(|t| (t.name().to_owned(), t))
        .collect();
    let pmt_tools::Executor::Handler(handler) = &tools["web_search"].executor else {
        panic!("web_search runs a handler");
    };
    for call in ["c1", "c2"] {
        let context = pmt_tools::CallContext {
            call_id: call.into(),
            session_id: "ses_usage".into(),
            cancel: pmt_core::cancel::CancelToken::new(),
        };
        let result = handler
            .call(&context, json!({"query": "metered"}))
            .await
            .unwrap();
        assert_eq!(result.status, Status::Ok);
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].purpose, "tool.web_search");
    assert_eq!(seen[0].provider, "tavily");
    assert_eq!(seen[0].billing_source, "api_key:tavily");
}
