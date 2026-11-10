//! Mixture of Agents against local servers (spec: mixture-of-agents, MoA
//! call procedure; Cache-safe guidance injection; Strict-alternation
//! recovery; Advisor cadence; Reference failures are non-fatal; Preset
//! validation; MoA virtual provider and presets).

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_providers::capability::Needs;
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::{self, Fallback, Healthy, Walk};
use pmt_providers::moa::Mixture;
use pmt_providers::request::{Message, Part, Request, Role, Tool};
use pmt_providers::resolve::{Target, Trace};
use pmt_providers::{CallError, Called, ModelCaller, Route};
use serde_json::Value;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use crate::support::{Env, bodies, caller, request, sse};

/// Custom providers `ra`, `rb`, `agg` (chat) and `ant` (Anthropic) on
/// `uri`; `[moa.presets.review]` refers to `ra` and `rb` and aggregates on
/// `agg`, plus `preset` lines.
pub(crate) fn config(uri: &str, preset: &str, extra: &str) -> String {
    let mut text = String::new();
    for id in ["ra", "rb", "agg", "prim"] {
        let _ = write!(
            text,
            "[providers.{id}]\nbase_url = \"{uri}/{id}/v1\"\nmax_retries = 0\n"
        );
    }
    let _ = write!(
        text,
        "[providers.ant]\nbase_url = \"{uri}/ant\"\nwire_format = \"anthropic_messages\"\nmax_retries = 0\n\
         [moa.presets.review]\nreference_models = [{{provider = \"ra\", model = \"m\"}}, {{provider = \"rb\", model = \"m\"}}]\n\
         aggregator = {{provider = \"agg\", model = \"m\"}}\n{preset}\n{extra}"
    );
    text
}

/// Mounts `response` for `provider`'s path prefix.
pub(crate) async fn respond(server: &MockServer, provider: &str, response: impl Respond + 'static) {
    Mock::given(method("POST"))
        .and(path_regex(format!("^/{provider}/")))
        .respond_with(response)
        .mount(server)
        .await;
}

/// A chat completion streaming `text`.
pub(crate) fn chat_text(text: &str) -> ResponseTemplate {
    let chunk = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
    });
    let usage = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!(
            "data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
        ))
}

/// A request with the session's system prompt and `tools` tool schemas.
pub(crate) fn with_tools(user: &str, tools: usize) -> Request {
    let mut req = request(user);
    req.tools = (0..tools)
        .map(|i| Tool {
            name: format!("tool_{i}"),
            description: "a tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        })
        .collect();
    req
}

/// A ledger context of `session`'s turn `turn`.
pub(crate) fn turn_ctx(session: SessionId, turn: &str) -> CallContext {
    let mut ctx = CallContext::new(session, "main");
    ctx.turn_id = Some(turn.to_owned());
    ctx
}

/// Resolves `provider/model` and calls it once with the session's
/// fallback chain, reading the stream to its end.
pub(crate) async fn call(
    env: &Env,
    caller: &ModelCaller,
    target: (&str, &str),
    req: &Request,
    ctx: Option<&CallContext>,
) -> Result<Called, CallError> {
    let resolver = env.resolver();
    let route = resolver
        .resolve_target(
            &Target {
                provider: target.0.into(),
                model: Some(target.1.into()),
                ..Target::default()
            },
            &mut Trace::default(),
        )
        .await
        .unwrap();
    let chain = fallback::chain(&env.config, &env.registry).unwrap();
    let mut state = Fallback::new(route, chain, 0.0);
    let walk = Walk {
        resolver: &resolver,
        health: &Healthy,
        catalog: &env.catalog,
        needs: Needs::of(req),
    };
    let mut outcome = caller.call(&mut state, &walk, req, ctx).await?;
    outcome.stream.collect().await;
    Ok(outcome)
}

/// The JSON bodies posted under `prefix`.
pub(crate) async fn posted(server: &MockServer, prefix: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn roles(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_owned())
        .collect()
}

fn last_text(body: &Value) -> String {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    match &last["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Scenario: References never see tools; Temperature omitted by default.
#[tokio::test]
async fn references_never_see_tools_or_the_system_prompt() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("use a queue")).await;
    respond(&server, "rb", chat_text("use a lock")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), "", ""), &[]);
    let req = with_tools("design it", 12);
    let outcome = call(&env, &caller(), ("moa", "review"), &req, None)
        .await
        .unwrap();
    assert_eq!(outcome.route.label(), "moa/review");
    for reference in ["/ra/v1/chat/completions", "/rb/v1/chat/completions"] {
        let sent = bodies(&server, reference).await;
        assert_eq!(sent.len(), 1, "{reference}");
        assert!(sent[0].get("tools").is_none(), "{}", sent[0]);
        assert_eq!(roles(&sent[0]), ["user"], "no system prompt");
        assert!(sent[0].get("temperature").is_none(), "temperature omitted");
    }
    let aggregated = bodies(&server, "/agg/v1/chat/completions").await;
    assert_eq!(aggregated[0]["tools"].as_array().unwrap().len(), 12);
    assert_eq!(roles(&aggregated[0]), ["system", "user", "user"]);
    let guidance = last_text(&aggregated[0]);
    assert!(
        guidance.contains("[advisor 1: ra/m]\nuse a queue"),
        "{guidance}"
    );
    assert!(
        guidance.contains("[advisor 2: rb/m]\nuse a lock"),
        "{guidance}"
    );
    assert!(aggregated[0].get("temperature").is_none());
    assert_eq!(outcome.advice.len(), 2);
}

#[tokio::test]
async fn a_set_temperature_is_sent_and_a_disabled_preset_calls_the_aggregator_alone() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    let env = Env::new(
        &config(
            &server.uri(),
            "reference_temperature = 0.3\n",
            "[moa.presets.solo]\nreference_models = [{provider = \"ra\", model = \"m\"}]\n\
             aggregator = {provider = \"agg\", model = \"m\"}\nenabled = false\naggregator_temperature = 0.7\n",
        ),
        &[],
    );
    call(&env, &caller(), ("moa", "review"), &request("x"), None)
        .await
        .unwrap();
    assert_eq!(
        bodies(&server, "/ra/v1/chat/completions").await[0]["temperature"],
        0.3
    );
    call(&env, &caller(), ("moa", "solo"), &request("x"), None)
        .await
        .unwrap();
    assert_eq!(
        bodies(&server, "/ra/v1/chat/completions").await.len(),
        1,
        "no references"
    );
    let solo = bodies(&server, "/agg/v1/chat/completions").await;
    assert_eq!(roles(&solo[1]), ["system", "user"], "no guidance");
    assert_eq!(solo[1]["temperature"], 0.7);
}

/// Scenario: One advisor missing credentials.
#[tokio::test]
async fn an_advisor_without_credentials_becomes_a_note() {
    let server = MockServer::start().await;
    respond(&server, "rb", chat_text("use a lock")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    // The first reference is on DeepSeek, whose key the profile lacks.
    let text = config(&server.uri(), "", "").replacen(
        "{provider = \"ra\", model = \"m\"}",
        "{provider = \"deepseek\", model = \"m\"}",
        1,
    );
    let env = Env::new(&text, &[]);
    let outcome = call(&env, &caller(), ("moa", "review"), &request("x"), None)
        .await
        .unwrap();
    assert!(posted(&server, "/ra/").await.is_empty());
    let guidance = last_text(&bodies(&server, "/agg/v1/chat/completions").await[0]);
    assert!(
        guidance.contains("[advisor 1: deepseek/m] unavailable:"),
        "{guidance}"
    );
    assert!(guidance.contains("DEEPSEEK_API_KEY"), "{guidance}");
    assert!(
        guidance.contains("[advisor 2: rb/m]\nuse a lock"),
        "{guidance}"
    );
    assert!(outcome.stream.failure().is_none(), "the turn completes");
}

/// Answers 400 `Conversation roles must alternate` to two consecutive
/// user messages, else a text completion.
struct Alternating;

impl Respond for Alternating {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let roles = roles(&body);
        if roles.ends_with(&["user".to_owned(), "user".to_owned()]) {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "Conversation roles must alternate user/assistant/user/assistant/...", "type": "BadRequestError"}
            }));
        }
        chat_text("done")
    }
}

/// Scenario: Merge remembered per destination.
#[tokio::test]
async fn an_alternating_destination_is_merged_for_the_rest_of_the_session() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", Alternating).await;
    let env = Env::new(&config(&server.uri(), "", ""), &[]);
    let caller = caller().with_mixture(Arc::new(Mixture::default()));
    let session = SessionId::new();
    for turn in ["t1", "t2", "t3"] {
        let outcome = call(
            &env,
            &caller,
            ("moa", "review"),
            &request("plan"),
            Some(&turn_ctx(session, turn)),
        )
        .await
        .unwrap();
        assert!(outcome.stream.failure().is_none());
    }
    let sent = bodies(&server, "/agg/v1/chat/completions").await;
    // Turn 1: refused split, retried merged; turns 2 and 3: merged at once.
    assert_eq!(sent.len(), 4);
    assert_eq!(roles(&sent[0]), ["system", "user", "user"]);
    for merged in &sent[1..] {
        assert_eq!(roles(merged), ["system", "user"]);
        let text = last_text(merged);
        assert!(text.starts_with("plan\n\n<moa-guidance>"), "{text}");
    }
    // Another session starts on the split shape.
    call(
        &env,
        &caller,
        ("moa", "review"),
        &request("plan"),
        Some(&turn_ctx(SessionId::new(), "t1")),
    )
    .await
    .unwrap();
    let sent = bodies(&server, "/agg/v1/chat/completions").await;
    assert_eq!(roles(&sent[4]), ["system", "user", "user"]);
}

/// The second iteration of a turn: the first request plus the model's
/// tool call and its result.
fn second_iteration(first: &Request) -> Request {
    let mut next = first.clone();
    next.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall {
            id: "toolu_01".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"src/main.rs\"}".into(),
        }],
        cache_breakpoint: false,
    });
    next.messages.push(Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "toolu_01".into(),
            content: "fn main() {}".into(),
            is_error: false,
        }],
        cache_breakpoint: false,
    });
    next
}

/// Scenario: Prefix preserved across iterations.
#[tokio::test]
async fn the_second_aggregator_request_keeps_the_first_ones_prefix() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_tool_call.sse")).await;
    respond(&server, "ant", sse("anthropic_tool_call.sse")).await;
    let text = config(
        &server.uri(),
        "",
        "[moa.presets.claude]\nreference_models = [{provider = \"ra\", model = \"m\"}]\naggregator = {provider = \"ant\", model = \"m\"}\n",
    );
    let env = Env::new(&text, &[]);
    let caller = caller();
    let session = SessionId::new();
    let first = request("fix main");
    let second = second_iteration(&first);
    for (preset, turn) in [("review", "t1"), ("claude", "t2")] {
        let ctx = turn_ctx(session, turn);
        for req in [&first, &second] {
            call(&env, &caller, ("moa", preset), req, Some(&ctx))
                .await
                .unwrap();
        }
    }
    // Chat Completions: the guidance is its own trailing message.
    let chat = bodies(&server, "/agg/v1/chat/completions").await;
    let (one, two) = (&chat[0], &chat[1]);
    assert_eq!(one["tools"], two["tools"]);
    let one_messages = one["messages"].as_array().unwrap();
    let two_messages = two["messages"].as_array().unwrap();
    let kept = one_messages.len() - 1;
    assert_eq!(one_messages[..kept], two_messages[..kept], "prefix bytes");
    assert_eq!(
        one_messages.last(),
        two_messages.last(),
        "same guidance reused"
    );
    assert_eq!(roles(two), ["system", "user", "assistant", "tool", "user"]);
    // Anthropic: a separate block of the user turn; the user's block stays.
    let anthropic = bodies(&server, "/ant/v1/messages").await;
    let (one, two) = (&anthropic[0], &anthropic[1]);
    assert_eq!(one["system"], two["system"]);
    assert_eq!(one["tools"], two["tools"]);
    let first_turn = &one["messages"][0]["content"];
    assert_eq!(
        first_turn.as_array().unwrap().len(),
        2,
        "user block + guidance"
    );
    assert_eq!(two["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(two["messages"][0]["content"][0], first_turn[0]);
    let results = two["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results[0]["type"], "tool_result");
    assert_eq!(results.last().unwrap(), &first_turn[1], "guidance trails");
}

async fn reference_runs(fanout: &str, iterations: usize) -> (usize, Vec<Value>) {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), fanout, ""), &[]);
    let caller = caller().with_mixture(Arc::new(Mixture::default()));
    let ctx = turn_ctx(SessionId::new(), "t1");
    for _ in 0..iterations {
        call(&env, &caller, ("moa", "review"), &request("x"), Some(&ctx))
            .await
            .unwrap();
    }
    let runs = posted(&server, "/ra/").await.len();
    (runs, posted(&server, "/agg/").await)
}

/// Scenario: Every third iteration.
#[tokio::test]
async fn every_third_iteration_runs_the_references_and_every_request_has_guidance() {
    let (runs, aggregated) = Box::pin(reference_runs("fanout = \"every_n:3\"\n", 7)).await;
    assert_eq!(runs, 3, "iterations 1, 3 and 6");
    assert_eq!(aggregated.len(), 7);
    assert!(
        aggregated
            .iter()
            .all(|b| last_text(b).contains("<moa-guidance>"))
    );
    let (runs, _) = Box::pin(reference_runs(
        "fanout = { mode = \"every_n\", n = 3 }\n",
        7,
    ))
    .await;
    assert_eq!(runs, 3);
    let (runs, _) = Box::pin(reference_runs("fanout = \"per_iteration\"\n", 3)).await;
    assert_eq!(runs, 3);
}

struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Scenario: Malformed fanout.
#[tokio::test]
async fn a_malformed_fanout_warns_naming_the_preset_and_runs_once_per_turn() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || Sink(sink.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (runs, _) = Box::pin(reference_runs("fanout = \"sometimes\"\n", 3)).await;
    assert_eq!(runs, 1);
    let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let warnings: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("WARN") && l.contains("fanout"))
        .collect();
    assert_eq!(warnings.len(), 1, "{logs}");
    assert!(warnings[0].contains("moa.presets.review.fanout"), "{logs}");
}

/// Scenario: Unknown preset.
#[tokio::test]
async fn an_unknown_preset_lists_the_configured_ones() {
    let env = Env::new(&config("http://127.0.0.1:9", "", ""), &[]);
    let err = env
        .resolver()
        .resolve_target(
            &Target {
                provider: "moa".into(),
                model: Some("nope".into()),
                ..Target::default()
            },
            &mut Trace::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "model.unknown");
    assert!(
        err.message().contains("presets: default, review"),
        "{}",
        err.message()
    );
    // The alias resolves too, and a preset route stands for its aggregator.
    let route: Route = env
        .resolver()
        .resolve_target(
            &Target {
                provider: "mixture-of-agents".into(),
                model: Some("review".into()),
                ..Target::default()
            },
            &mut Trace::default(),
        )
        .await
        .unwrap();
    assert_eq!(route.label(), "moa/review");
    assert_eq!(route.acting().label(), "agg/m");
    assert_eq!(route.billing_source(), "local", "the aggregator's");
}
