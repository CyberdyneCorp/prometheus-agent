//! The built-in `mock` provider end to end through the resolver and the
//! model caller (spec: replay-and-evaluation, Deterministic mock provider;
//! Mock fault injection): resolution without credentials or network, echo
//! mode, scripted conversations, byte-identical replays, rule matchers,
//! usage and cost, and every injected fault seen by retries and the
//! fallback chain as its classified error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_core::error::codes;
use pmt_obs::usage::UsageRecorder;
use pmt_providers::capability::Needs;
use pmt_providers::classify::{ErrorClass, ProviderError};
use pmt_providers::cost::CallContext;
use pmt_providers::event::{StopReason, Usage};
use pmt_providers::fallback::{self, Fallback, Healthy, Walk};
use pmt_providers::mock::{Mock, Script};
use pmt_providers::request::{Message, Part, Request, Role};
use pmt_providers::resolve::{Target, Trace};
use pmt_providers::{ModelCaller, Route, StreamEvent};
use pmt_store::{NewSession, Source};

use crate::support::{Env, caller, request};

fn scripted(script: &str) -> ModelCaller {
    caller().with_mock(Arc::new(Mock::new(
        Script::parse(script, "s.toml").unwrap(),
    )))
}

async fn mock_route(env: &Env, model: &str) -> Route {
    let mut route = env.route("mock", model).await;
    route.retry.max_retries = 0;
    route
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn stop_of(events: &[StreamEvent]) -> StopReason {
    match events.last() {
        Some(StreamEvent::Stop { stop_reason }) => *stop_reason,
        other => panic!("no stop: {other:?}"),
    }
}

fn usage_of(events: &[StreamEvent]) -> Usage {
    events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage { usage } => Some(*usage),
            _ => None,
        })
        .unwrap()
}

async fn answer(caller: &ModelCaller, route: &Route, req: &Request) -> Vec<StreamEvent> {
    caller.send(route, req, None).await.unwrap().collect().await
}

async fn failure(caller: &ModelCaller, route: &Route, req: &Request) -> ProviderError {
    caller.send(route, req, None).await.unwrap_err()
}

/// `summarise`, the model's `read_file` call and its result.
fn after_tool_call(result: &str) -> Request {
    let mut req = request("summarise");
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall {
            id: "call_0_0".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
        }],
        cache_breakpoint: false,
    });
    req.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "call_0_0".into(),
            content: result.into(),
            is_error: false,
        }],
        cache_breakpoint: false,
    });
    req
}

#[tokio::test]
async fn mock_resolves_without_credentials_or_network_and_echoes() {
    let env = Env::new("", &[]);
    let target = Target {
        provider: "mock".into(),
        ..Target::default()
    };
    let route = env
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap();
    assert_eq!(route.model, "mock");
    assert_eq!(route.base_url, "mock://local");
    assert_eq!(route.credential.describe(), "none");
    assert_eq!(route.billing_source(), "keyless:mock");

    let events = answer(&caller(), &route, &request("hi")).await;
    assert_eq!(text_of(&events), "mock: hi");
    assert_eq!(stop_of(&events), StopReason::EndTurn);
    // ceil(bytes / 4) per message: system blocks of 28 and 14 bytes and
    // "hi" in; "mock: hi" out.
    let usage = usage_of(&events);
    assert_eq!((usage.input_tokens, usage.output_tokens), (7 + 4 + 1, 2));
    assert_eq!(usage.cost_usd, Some(0.0));
}

#[tokio::test]
async fn a_script_plays_a_tool_call_then_its_answer() {
    let env = Env::new("", &[]);
    let route = mock_route(&env, "mock").await;
    let caller = scripted(
        r#"
[[rule]]
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "README.md" }

[[rule]]
text = "done"
"#,
    );
    let events = answer(&caller, &route, &request("summarise")).await;
    assert_eq!(
        events[..3],
        [
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_0_0".into(),
                name: "read_file".into()
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments: r#"{"path":"README.md"}"#.into()
            },
            StreamEvent::ToolCallEnd {
                index: 0,
                id: "call_0_0".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"README.md"}"#.into()
            },
        ]
    );
    assert_eq!(stop_of(&events), StopReason::ToolUse);

    let events = answer(&caller, &route, &after_tool_call("# Demo")).await;
    assert_eq!(text_of(&events), "done");
    assert_eq!(stop_of(&events), StopReason::EndTurn);

    // Every rule is used: the script is over.
    let error = failure(&caller, &route, &request("again")).await;
    assert_eq!(error.class, ErrorClass::Config);
    assert!(
        error.message.contains("no unused rule"),
        "{}",
        error.message
    );
    assert_eq!(
        error.to_error("mock").code().code,
        codes::PROVIDER_INVALID_REQUEST.code
    );
}

const REPLAYED: &str = r#"
[[rule]]
reasoning = "Look at the file first."
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "README.md", limit = 20 }
[[rule.tool_calls]]
id = "custom"
name = "terminal"
arguments = '{"command": "ls"}'

[[rule]]
fault = "rate_limit"
retry_after_secs = 0

[[rule]]
step = 1
text = "Two files: README.md and ledger.py."
"#;

/// The serialised streams of one scripted conversation, retries included.
async fn conversation() -> Vec<u8> {
    let env = Env::new("", &[]);
    let mut route = mock_route(&env, "mock").await;
    route.retry.max_retries = 1;
    let caller = scripted(REPLAYED);
    let mut streams = Vec::new();
    for req in [
        request("list the files"),
        after_tool_call("README.md\nledger.py"),
    ] {
        streams.push(answer(&caller, &route, &req).await);
    }
    serde_json::to_vec(&streams).unwrap()
}

#[tokio::test]
async fn two_runs_of_the_same_script_stream_byte_identical_events() {
    let first = conversation().await;
    let second = conversation().await;
    assert_eq!(first, second);
    let text = String::from_utf8(first).unwrap();
    for expected in [
        r#"{"type":"reasoning_delta","text":"first."}"#,
        r#""id":"custom","name":"terminal""#,
        r#"{"type":"text_delta","text":"ledger.py."}"#,
    ] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
}

#[tokio::test]
async fn rules_match_on_step_model_user_message_and_tool_result() {
    let env = Env::new("", &[]);
    let caller = scripted(
        r#"
[[rule]]
model = "other"
text = "other model"
repeat = true
[[rule]]
step = 1
tool_result_regex = "^ok"
text = "after a good tool result"
repeat = true
[[rule]]
user = "weather"
text = "sunny"
repeat = true
[[rule]]
user_regex = "(?i)^hello"
text = "hi there"
repeat = true
"#,
    );
    let route = mock_route(&env, "mock").await;
    let other = mock_route(&env, "other").await;
    // A failed tool result does not match its rule; the user message
    // still matches another.
    let mut failed_tool = after_tool_call("error");
    failed_tool.messages[0] = Message::text(Role::User, "weather today");
    let cases = [
        (&other, request("hello"), "other model"),
        (&route, request("what's the weather?"), "sunny"),
        (&route, request("HELLO you"), "hi there"),
        (
            &route,
            after_tool_call("ok: 3 lines"),
            "after a good tool result",
        ),
        (&route, failed_tool, "sunny"),
    ];
    for (route, req, expected) in cases {
        let events = answer(&caller, route, &req).await;
        assert_eq!(text_of(&events), expected);
    }
    let error = failure(&caller, &route, &request("nothing matches")).await;
    assert_eq!(error.class, ErrorClass::Config);
}

#[tokio::test]
async fn usage_counts_each_message_and_the_reply_and_costs_nothing() {
    let env = Env::new("", &[]);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = crate::caching::store(&dir.path().join("home"));
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let catalog = Arc::new(pmt_providers::metadata::Catalog::new(&env.config, None));
    let caller = scripted("[[rule]]\nreasoning = \"12345\"\ntext = \"abc\"\n")
        .with_recorder(Arc::new(UsageRecorder::new(store.clone(), catalog)));
    let route = mock_route(&env, "mock").await;
    let req = after_tool_call("0123456789");
    let mut stream = caller
        .send(&route, &req, Some(&CallContext::new(session, "main")))
        .await
        .unwrap();
    let events = stream.collect().await;
    let usage = usage_of(&events);
    // In: 28 and 14 system bytes, "summarise" (9), the call (9 + 20),
    // the result (10). Out: 5 reasoning + 3 text bytes.
    assert_eq!(usage.input_tokens, 7 + 4 + 3 + 8 + 3);
    assert_eq!((usage.output_tokens, usage.reasoning_tokens), (2, 2));
    let row = stream.recorded().unwrap().as_ref().unwrap();
    assert_eq!(row.billing_source, "keyless:mock");
    assert_eq!(row.cost_usd, Some(0.0));
}

#[tokio::test]
async fn each_injected_fault_reaches_the_caller_as_its_classified_error() {
    let env = Env::new("", &[]);
    let route = mock_route(&env, "mock").await;
    let caller = scripted(
        r#"
[[rule]]
fault = "rate_limit"
retry_after_secs = 30
[[rule]]
fault = "timeout"
[[rule]]
fault = "server_error"
status = 502
[[rule]]
fault = "auth_error"
[[rule]]
fault = "context_overflow"
[[rule]]
fault = "malformed_stream"
"#,
    );
    let expected = [
        (ErrorClass::RateLimit, codes::PROVIDER_CALL_FAILED.code, 69),
        (ErrorClass::Timeout, codes::PROVIDER_CALL_FAILED.code, 69),
        (
            ErrorClass::ServerError,
            codes::PROVIDER_CALL_FAILED.code,
            69,
        ),
        (ErrorClass::Auth, codes::PROVIDER_AUTH_FAILED.code, 78),
        (
            ErrorClass::ContextOverflow,
            codes::PROVIDER_CALL_FAILED.code,
            69,
        ),
        (
            ErrorClass::ServerError,
            codes::PROVIDER_CALL_FAILED.code,
            69,
        ),
    ];
    for (class, code, exit) in expected {
        let error = failure(&caller, &route, &request("x")).await;
        assert_eq!(error.class, class, "{}", error.message);
        let user_error = error.to_error("mock");
        assert_eq!(
            (user_error.code().code, user_error.exit_code()),
            (code, exit)
        );
        if class == ErrorClass::RateLimit {
            assert_eq!(error.retry_after, Some(Duration::from_secs(30)));
        }
    }
}

#[tokio::test]
async fn a_stream_fault_after_content_ends_the_stream_with_an_error_stop() {
    let env = Env::new("", &[]);
    let route = mock_route(&env, "mock").await;
    let caller = scripted(
        "[[rule]]\ntext = \"partial answer\"\nfault = \"truncated_stream\"\n[[rule]]\ntext = \"partial\"\nfault = \"malformed_stream\"\n",
    );
    for class in [ErrorClass::Network, ErrorClass::ServerError] {
        let mut stream = caller.send(&route, &request("x"), None).await.unwrap();
        let events = stream.collect().await;
        assert!(text_of(&events).starts_with("partial"));
        assert_eq!(stop_of(&events), StopReason::Error);
        assert_eq!(stream.failure().unwrap().class, class);
    }
}

#[tokio::test]
async fn retryable_faults_are_retried_on_the_same_route() {
    let env = Env::new("", &[]);
    let mut route = env.route("mock", "mock").await;
    route.retry.max_retries = 3;
    let script = r#"
[[rule]]
fault = "rate_limit"
retry_after_secs = 0
times = 2
[[rule]]
fault = "server_error"
status = 503
[[rule]]
text = "recovered"
"#;
    let events = answer(&scripted(script), &route, &request("x")).await;
    assert_eq!(text_of(&events), "recovered");

    // The attempt cap stops before the script recovers.
    let capped = scripted(script).with_attempt_cap(2);
    let error = failure(&capped, &route, &request("x")).await;
    assert_eq!(error.class, ErrorClass::RateLimit);

    // An auth fault is never retried: the next rule is left unused.
    let caller = scripted("[[rule]]\nfault = \"auth_error\"\n[[rule]]\ntext = \"unused\"\n");
    assert_eq!(
        failure(&caller, &route, &request("x")).await.class,
        ErrorClass::Auth
    );
    let events = answer(&caller, &route, &request("x")).await;
    assert_eq!(text_of(&events), "unused");
}

/// Spec scenario "Fallback on rate limit": `mock/primary` is rate limited
/// on every call, so the call completes on `mock/secondary` and the
/// fallback is reported for the trajectory.
#[tokio::test]
async fn a_rate_limited_primary_falls_back_to_the_secondary() {
    let env = Env::new(
        "[[fallback.chain]]\nprovider = \"mock\"\nmodel = \"secondary\"\n",
        &[],
    );
    let caller = scripted(
        r#"
[[rule]]
model = "primary"
fault = "rate_limit"
repeat = true
[[rule]]
model = "secondary"
text = "answered by the secondary"
repeat = true
"#,
    );
    let primary = env.route("mock", "primary").await;
    let chain = fallback::chain(&env.config, &env.registry).unwrap();
    let mut state = Fallback::new(primary, chain, env.config.fallback.min_health);
    let resolver = env.resolver();
    let req = request("x");
    let walk = Walk {
        resolver: &resolver,
        health: &Healthy,
        catalog: &env.catalog,
        needs: Needs::of(&req),
    };
    let mut answered = caller.call(&mut state, &walk, &req, None).await.unwrap();
    assert_eq!(answered.route.label(), "mock/secondary");
    assert_eq!(
        text_of(&answered.stream.collect().await),
        "answered by the secondary"
    );
    let [activated] = answered.fallbacks.as_slice() else {
        panic!("one fallback: {:?}", answered.fallbacks)
    };
    let payload = activated.payload();
    assert_eq!(
        (
            payload.from.as_str(),
            payload.to.as_str(),
            payload.reason.as_str()
        ),
        ("mock/primary", "mock/secondary", "rate_limit")
    );
}

#[tokio::test]
async fn chunk_latency_is_bounded_by_the_stream_idle_timeout() {
    let env = Env::new("", &[]);
    let mut route = mock_route(&env, "mock").await;
    let caller = scripted(
        "latency_ms = 10\n[[rule]]\ntext = \"a b\"\n[[rule]]\ntext = \"slow\"\nlatency_ms = 500\n",
    );
    let start = Instant::now();
    let events = answer(&caller, &route, &request("x")).await;
    assert_eq!(text_of(&events), "a b");
    // Two words and the end, each after 10 ms.
    assert!(start.elapsed() >= Duration::from_millis(30));

    route.timeouts.idle = Duration::from_millis(50);
    let error = failure(&caller, &route, &request("x")).await;
    assert_eq!(error.class, ErrorClass::Timeout);
    assert!(error.message.contains("idle timeout"), "{}", error.message);
}

#[tokio::test]
async fn a_session_takes_the_flag_script_then_the_configured_one_else_echoes() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(profile.join("scripts")).unwrap();
    std::fs::write(
        profile.join("scripts/s.toml"),
        "[[rule]]\ntext = \"configured\"\n",
    )
    .unwrap();
    let flag = dir.path().join("flag.toml");
    std::fs::write(&flag, "[[rule]]\ntext = \"from the flag\"\n").unwrap();
    let config: pmt_config::Config =
        toml::from_str("[providers.mock]\nscript = \"scripts/s.toml\"\n").unwrap();
    let env = Env::new("", &[]);
    let route = mock_route(&env, "mock").await;
    let play = async |mock: Mock| {
        let caller = caller().with_mock(Arc::new(mock));
        text_of(&answer(&caller, &route, &request("x")).await)
    };
    let mock = Mock::for_session(Some(&flag), &config, &profile).unwrap();
    assert_eq!(play(mock).await, "from the flag");
    let mock = Mock::for_session(None, &config, &profile).unwrap();
    assert_eq!(play(mock).await, "configured");
    let mock = Mock::for_session(None, &pmt_config::Config::default(), &profile).unwrap();
    assert_eq!(play(mock).await, "mock: x");

    std::fs::write(&flag, "[[rule]]\nbad = 1\n").unwrap();
    let err = Mock::for_session(Some(&flag), &config, &profile).unwrap_err();
    assert_eq!(err.code().code, codes::MOCK_SCRIPT_INVALID.code);
    assert!(err.message().contains("flag.toml"), "{}", err.message());
    assert_eq!(err.exit_code(), 78);
    let err = Mock::for_session(None, &config, &dir.path().join("elsewhere")).unwrap_err();
    assert_eq!(err.code().code, codes::MOCK_SCRIPT_MISSING.code);
}
