//! Prompt-cache markers on the wire and per-call ledger rows (spec:
//! Prompt caching and stable prefix across route changes; Per-call cost
//! accounting; observability, Per-call usage ledger).

use std::sync::Arc;

use pmt_config::schema::{PromptCacheConfig, PromptCacheTtl};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_obs::usage::UsageRecorder;
use pmt_providers::cost::CallContext;
use pmt_providers::request::{CacheControl, Message, Role, prompt_cache_key};
use pmt_providers::transport;
use pmt_store::{CostStatus, NewSession, Source, Store};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::support::{Env, bodies, caller, request, sse};

const SESSION: &str = "ses_01JZ0000000000000000000000";

fn cache(ttl: PromptCacheTtl) -> CacheControl {
    CacheControl::for_session(&PromptCacheConfig { ttl }, SESSION)
}

fn uri_config(uri: &str) -> String {
    format!(
        "[providers.ant]\nbase_url = \"{uri}/ant\"\nwire_format = \"anthropic_messages\"\nkey_secret = \"ANT_KEY\"\n\
         [providers.resp]\nbase_url = \"{uri}/resp/v1\"\nwire_format = \"responses\"\nkey_secret = \"RESP_KEY\"\nsupports_prompt_cache = true\n\
         [providers.plain]\nbase_url = \"{uri}/plain/v1\"\nkey_secret = \"PLAIN_KEY\"\n\
         [models.\"ant/m\".price]\ninput = 3.0\noutput = 15.0\ncache_read = 0.3\ncache_write = 3.75\n"
    )
}

const KEYS: &[(&str, &str)] = &[
    ("ANT_KEY", "ant-key-0123456789"),
    ("RESP_KEY", "resp-key-0123456789"),
    ("PLAIN_KEY", "plain-key-0123456789"),
];

#[tokio::test]
async fn anthropic_requests_carry_cache_control_on_each_breakpoint() {
    let env = Env::new(&uri_config("http://127.0.0.1:9"), KEYS);
    let route = env.route("ant", "m").await;
    let mut req = request("hello");
    req.messages.push(Message::text(Role::Assistant, "hi"));
    req.messages.push(Message::text(Role::User, "and now?"));
    req.messages[2].cache_breakpoint = true;
    req.cache = cache(PromptCacheTtl::FiveMinutes);
    let body = transport::body(&route, &req);
    let marker = json!({"type": "ephemeral"});
    assert_eq!(body["system"][0]["cache_control"], marker);
    assert_eq!(body["system"][1]["cache_control"], marker);
    assert_eq!(body["tools"][0]["cache_control"], marker);
    assert_eq!(body["messages"][2]["content"][0]["cache_control"], marker);
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(body.get("prompt_cache_key").is_none());

    req.cache = cache(PromptCacheTtl::OneHour);
    let body = transport::body(&route, &req);
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    // At most four breakpoints: the oldest message breakpoint is dropped.
    req.messages[0].cache_breakpoint = true;
    let body = transport::body(&route, &req);
    assert_eq!(body.to_string().matches("cache_control").count(), 4);
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );

    req.cache = cache(PromptCacheTtl::Off);
    assert!(
        !transport::body(&route, &req)
            .to_string()
            .contains("cache_control")
    );
}

#[tokio::test]
async fn cache_key_is_stable_across_turns_and_only_sent_where_supported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&uri_config(&server.uri()), KEYS);
    let route = env.route("resp", "m").await;
    let mut req = request("turn one");
    req.cache = cache(PromptCacheTtl::Auto);
    caller()
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    req.messages.push(Message::text(Role::Assistant, "done"));
    req.messages.push(Message::text(Role::User, "turn two"));
    caller()
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    let sent = bodies(&server, "/responses").await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0]["prompt_cache_key"],
        json!(prompt_cache_key(SESSION))
    );
    assert_eq!(sent[0]["prompt_cache_key"], sent[1]["prompt_cache_key"]);

    let plain = env.route("plain", "m").await;
    assert!(
        transport::body(&plain, &req)
            .get("prompt_cache_key")
            .is_none()
    );
    req.cache = cache(PromptCacheTtl::Off);
    assert!(
        transport::body(&route, &req)
            .get("prompt_cache_key")
            .is_none()
    );
}

pub(crate) fn store(dir: &std::path::Path) -> Arc<Store> {
    let home = Home::open(dir).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

#[tokio::test]
async fn every_call_records_one_priced_ledger_row_with_its_billing_source() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ant/v1/messages"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&uri_config(&server.uri()), KEYS);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = store(&dir.path().join("home"));
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let catalog = Arc::new(pmt_providers::metadata::Catalog::new(&env.config, None));
    let recorder = Arc::new(UsageRecorder::new(store.clone(), catalog));
    let caller = caller().with_recorder(recorder);
    let ctx = CallContext::new(session, "main");
    let route = env.route("ant", "m").await;
    let req = request("hello");
    let mut stream = caller.send(&route, &req, Some(&ctx)).await.unwrap();
    stream.collect().await;
    let row = stream.recorded().unwrap().as_ref().unwrap().clone();
    assert_eq!(row.billing_source, "api_key:ant");
    assert_eq!(row.purpose, "main");
    assert_eq!(row.provider, "ant");
    assert_eq!(
        (
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_read,
            row.tokens.cache_write
        ),
        (120, 42, 4096, 2048)
    );
    assert_eq!(row.cost_status, CostStatus::Estimated);
    let expected = (120.0 * 3.0 + 42.0 * 15.0 + 4096.0 * 0.3 + 2048.0 * 3.75) / 1e6;
    assert!((row.cost_usd.unwrap() - expected).abs() < 1e-12);
    assert_eq!(row.request_hash, Some(req.hash("ant", "m")));
    assert_eq!(store.usage_events(session).unwrap().len(), 1);

    // A model without prices: unknown, never zero.
    let route = env.route("resp", "m").await;
    let mut stream = caller.send(&route, &req, Some(&ctx)).await.unwrap();
    stream.collect().await;
    let row = stream.recorded().unwrap().as_ref().unwrap();
    assert_eq!(row.cost_status, CostStatus::Unknown);
    assert_eq!(row.cost_usd, None);
    assert_eq!(store.usage_events(session).unwrap().len(), 2);
}
