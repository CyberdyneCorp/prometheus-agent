//! Fault injection: same-route retries, error classes that never reroute,
//! fallback activation with skips, turn-scoped restore, primary cooldowns,
//! the stable prefix across route changes and the stream idle timeout
//! (spec: Error classification and same-route retries; In-turn fallback
//! activation and turn-scoped restore; Prompt caching and stable prefix;
//! Capability-aware route validation; agent-turn-loop, Model call
//! retries).

use std::fmt::Write as _;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_core::id::SessionId;
use pmt_providers::capability::Needs;
use pmt_providers::classify::ErrorClass;
use pmt_providers::cost::{CallContext, StaleCap};
use pmt_providers::event::StopReason;
use pmt_providers::fallback::{self, Fallback, Healthy, RouteHealth, Walk};
use pmt_providers::request::{Part, Request};
use pmt_providers::{CallError, Route, StreamEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{Env, bodies, caller, fast, request, sse};

/// Providers `prim`, `b`, `c` (chat format) and `vis` on one server,
/// with no same-route retries unless a test sets them.
fn config(uri: &str, extra: &str) -> String {
    let mut text = String::new();
    for id in ["prim", "b", "c", "textonly", "vis"] {
        let _ = write!(
            text,
            "[providers.{id}]\nbase_url = \"{uri}/{id}/v1\"\nmax_retries = 0\n"
        );
    }
    text.push_str("[models.\"textonly/m\"]\nsupports_vision = false\n[models.\"vis/m\"]\nsupports_vision = true\n");
    text.push_str(extra);
    text
}

fn chain(entries: &[&str]) -> String {
    entries.iter().fold(String::new(), |mut text, p| {
        let _ = write!(
            text,
            "[[fallback.chain]]\nprovider = \"{p}\"\nmodel = \"m\"\n"
        );
        text
    })
}

async fn respond(server: &MockServer, provider: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path_regex(format!("^/{provider}/v1/")))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Mounts `responses` for `provider` in order (each used once, the last
/// one for every later call).
async fn sequence(server: &MockServer, provider: &str, responses: Vec<ResponseTemplate>) {
    let last = responses.len() - 1;
    for (i, response) in responses.into_iter().enumerate() {
        let mock = Mock::given(method("POST"))
            .and(path_regex(format!("^/{provider}/v1/")))
            .respond_with(response)
            .with_priority(u8::try_from(i + 1).unwrap());
        let mock = if i < last {
            mock.up_to_n_times(1)
        } else {
            mock
        };
        mock.mount(server).await;
    }
}

struct Session<'a> {
    env: &'a Env,
    state: Fallback,
}

impl<'a> Session<'a> {
    async fn new(env: &'a Env) -> Self {
        let primary = env.route("prim", "m").await;
        let chain = fallback::chain(&env.config, &env.registry).unwrap();
        Self {
            env,
            state: Fallback::new(primary, chain, env.config.fallback.min_health),
        }
    }

    async fn call(
        &mut self,
        req: &Request,
        health: &dyn RouteHealth,
    ) -> Result<pmt_providers::Called, pmt_providers::Failed> {
        match self.call_checked(req, health).await {
            Ok(called) => Ok(called),
            Err(CallError::Failed(failed)) => Err(*failed),
            Err(CallError::Refused(err)) => panic!("refused: {}", err.message()),
        }
    }

    async fn call_checked(
        &mut self,
        req: &Request,
        health: &dyn RouteHealth,
    ) -> Result<pmt_providers::Called, CallError> {
        let resolver = self.env.resolver();
        let walk = Walk {
            resolver: &resolver,
            health,
            catalog: &self.env.catalog,
            needs: Needs::of(req),
        };
        caller().call(&mut self.state, &walk, req, None).await
    }
}

#[tokio::test]
async fn retryable_failures_are_retried_on_the_same_route() {
    let server = MockServer::start().await;
    sequence(
        &server,
        "prim",
        vec![
            ResponseTemplate::new(503),
            ResponseTemplate::new(502),
            sse("chat_text.sse"),
        ],
    )
    .await;
    let env = Env::new(&config(&server.uri(), ""), &[]);
    let mut route = env.route("prim", "m").await;
    route.retry.max_retries = 2;
    let events = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.contains(&StreamEvent::Stop {
        stop_reason: StopReason::EndTurn
    }));
    assert_eq!(bodies(&server, "/chat/completions").await.len(), 3);
}

#[tokio::test]
async fn auth_errors_and_attempt_caps_are_not_retried() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(401)).await;
    respond(&server, "b", ResponseTemplate::new(503)).await;
    let env = Env::new(&config(&server.uri(), ""), &[]);
    let mut route = env.route("prim", "m").await;
    route.retry.max_retries = 3;
    let err = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Auth);
    assert_eq!(bodies(&server, "/prim/v1/chat/completions").await.len(), 1);
    // agent.api_max_retries = 1: one attempt, then the chain.
    let mut b = env.route("b", "m").await;
    b.retry.max_retries = 3;
    let err = caller()
        .with_attempt_cap(1)
        .send(&b, &request("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded);
    assert_eq!(bodies(&server, "/b/v1/chat/completions").await.len(), 1);
}

#[tokio::test]
async fn misconfiguration_is_never_rerouted() {
    let server = MockServer::start().await;
    let body = "{\"error\":{\"message\":\"tools.0.function.parameters: invalid schema\"}}";
    respond(
        &server,
        "prim",
        ResponseTemplate::new(400).set_body_string(body),
    )
    .await;
    respond(&server, "b", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), &chain(&["b"])), &[]);
    let mut session = Session::new(&env).await;
    let failed = session.call(&request("x"), &Healthy).await.unwrap_err();
    assert_eq!(failed.error.class, ErrorClass::InvalidRequest);
    assert!(failed.fallbacks.is_empty());
    assert_eq!(failed.to_error().code().code, "provider.invalid_request");
    assert!(bodies(&server, "/b/v1/chat/completions").await.is_empty());
}

#[tokio::test]
async fn plan_entitlement_is_classified_and_explained() {
    let server = MockServer::start().await;
    let body = "{\"error\":{\"message\":\"Your plan tier does not have API access\"}}";
    respond(
        &server,
        "prim",
        ResponseTemplate::new(403).set_body_string(body),
    )
    .await;
    let env = Env::new(&config(&server.uri(), ""), &[]);
    let route = env.route("prim", "m").await;
    let err = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Entitlement);
    let error = err.to_error("prim");
    assert!(error.hint().unwrap().contains("plan tier is not eligible"));
}

/// Every credential of `b` benched for two hours.
struct BenchedB;

impl RouteHealth for BenchedB {
    fn benched_for(&self, provider: &str, _now: Timestamp) -> Option<Duration> {
        (provider == "b").then_some(Duration::from_hours(2))
    }
}

#[tokio::test]
async fn candidate_with_an_exhausted_pool_is_skipped() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(529)).await;
    respond(&server, "b", sse("chat_text.sse")).await;
    respond(&server, "c", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), &chain(&["b", "c"])), &[]);
    let mut session = Session::new(&env).await;
    let called = session.call(&request("x"), &BenchedB).await.unwrap();
    assert_eq!(called.route.provider, "c");
    assert_eq!(called.fallbacks.len(), 1);
    let event = &called.fallbacks[0];
    assert_eq!(
        (event.from.provider.as_str(), event.to.provider.as_str()),
        ("prim", "c")
    );
    assert_eq!(event.reason, ErrorClass::Overloaded);
    assert_eq!(event.payload().to, "c/m");
    assert!(bodies(&server, "/b/v1/chat/completions").await.is_empty());
}

#[tokio::test]
async fn chain_exhausted_surfaces_the_primary_error_and_arms_a_short_cooldown() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(500)).await;
    let text = config(&server.uri(), &chain(&["b"])).replace(
        "/b/v1\"\nmax_retries = 0\n",
        "/b/v1\"\nmax_retries = 0\nenabled = false\n",
    );
    let env = Env::new(&text, &[]);
    let mut session = Session::new(&env).await;
    let before = Timestamp::now();
    let failed = session.call(&request("x"), &Healthy).await.unwrap_err();
    assert_eq!(failed.error.class, ErrorClass::ServerError);
    assert_eq!(failed.route.provider, "prim");
    let until = session.state.primary_cooldown_until().unwrap();
    let cooldown = until.duration_since(before);
    assert!(
        cooldown >= SignedDuration::from_secs(5) && cooldown < SignedDuration::from_secs(6),
        "{cooldown}"
    );
}

#[tokio::test]
async fn rate_limited_primary_stays_on_fallback_while_cooling_and_backs_off() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(429)).await;
    respond(&server, "c", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), &chain(&["c"])), &[]);
    let mut session = Session::new(&env).await;
    let mut expected = 60;
    for _ in 0..3 {
        let start = Timestamp::now();
        let called = session.call(&request("x"), &Healthy).await.unwrap();
        assert_eq!(called.route.provider, "c");
        let until = session.state.primary_cooldown_until().unwrap();
        let armed = until.duration_since(start).as_secs();
        assert!(
            (expected..=expected + 1).contains(&armed),
            "armed {armed}, expected {expected}"
        );
        // Next turn inside the cooldown keeps the fallback route.
        assert!(!session.state.start_turn(Timestamp::now()));
        assert_eq!(session.state.current().provider, "c");
        // Once the cooldown is over the primary is restored.
        let later = until.checked_add(SignedDuration::from_secs(1)).unwrap();
        assert!(session.state.start_turn(later));
        assert!(session.state.on_primary());
        expected *= 2;
    }
}

#[tokio::test]
async fn vision_request_skips_a_text_only_fallback() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(503)).await;
    respond(&server, "textonly", sse("chat_text.sse")).await;
    respond(&server, "vis", sse("chat_text.sse")).await;
    let env = Env::new(
        &config(
            &server.uri(),
            &format!(
                "[models.\"prim/m\"]\nsupports_vision = true\n{}",
                chain(&["textonly", "vis"])
            ),
        ),
        &[],
    );
    let mut session = Session::new(&env).await;
    let mut req = request("what is this?");
    req.messages[0].parts.push(Part::Image {
        media_type: "image/png".into(),
        data: "iVBORw0KGgo=".into(),
        is_url: false,
    });
    let called = session.call(&req, &Healthy).await.unwrap();
    assert_eq!(called.route.provider, "vis");
    assert!(
        bodies(&server, "/textonly/v1/chat/completions")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_route_without_tool_support_is_refused_for_a_tool_using_request() {
    let server = MockServer::start().await;
    respond(&server, "prim", sse("chat_text.sse")).await;
    let env = Env::new(
        &config(
            &server.uri(),
            &format!(
                "[models.\"prim/m\"]\nsupports_tools = false\n{}",
                chain(&["c"])
            ),
        ),
        &[],
    );
    let mut session = Session::new(&env).await;
    let Err(CallError::Refused(err)) = session.call_checked(&request("x"), &Healthy).await else {
        panic!("a tool-using request must be refused");
    };
    assert_eq!(err.code().code, "model.capability_missing");
    assert!(err.message().contains("supports_tools"));
    assert!(server.received_requests().await.unwrap().is_empty());
    // Without tools the same route is fine.
    let mut plain = request("x");
    plain.tools.clear();
    assert_eq!(
        session.call(&plain, &Healthy).await.unwrap().route.provider,
        "prim"
    );
}

#[tokio::test]
async fn restored_primary_replays_the_original_prefix() {
    let server = MockServer::start().await;
    sequence(
        &server,
        "prim",
        vec![
            sse("chat_text.sse"),
            ResponseTemplate::new(503),
            sse("chat_text.sse"),
        ],
    )
    .await;
    respond(&server, "c", sse("chat_text.sse")).await;
    let env = Env::new(&config(&server.uri(), &chain(&["c"])), &[]);
    let mut session = Session::new(&env).await;
    let mut req = request("turn one");
    for turn in 1..=3 {
        session.state.start_turn(Timestamp::now());
        let called = session.call(&req, &Healthy).await.unwrap();
        let expected = if turn == 2 { "c" } else { "prim" };
        assert_eq!(called.route.provider, expected, "turn {turn}");
        req.messages.push(pmt_providers::request::Message::text(
            pmt_providers::request::Role::Assistant,
            "ok",
        ));
        req.messages.push(pmt_providers::request::Message::text(
            pmt_providers::request::Role::User,
            format!("turn {}", turn + 1),
        ));
    }
    let primary = bodies(&server, "/prim/v1/chat/completions").await;
    let fallback = bodies(&server, "/c/v1/chat/completions").await;
    assert_eq!(primary.len(), 3);
    let prefix =
        |body: &serde_json::Value| (body["messages"][0].to_string(), body["tools"].to_string());
    assert_eq!(prefix(&primary[0]), prefix(&primary[2]));
    assert_eq!(prefix(&primary[0]), prefix(&fallback[0]));
    // Provider and model identity never enter the system prompt.
    assert!(!primary[0]["messages"][0].to_string().contains("prim"));
}

/// Serves HTTP 200 with one SSE event, then stays silent.
async fn stalling_server(first_event: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                let _ = socket.read(&mut buffer).await;
                let mut response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n".to_owned();
                if first_event {
                    let event =
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
                    let _ = write!(response, "{:x}\r\n{event}\r\n", event.len());
                }
                let _ = socket.write_all(response.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    format!("http://{address}")
}

async fn idle_route(base: &str) -> Route {
    let env = Env::new(
        &format!(
            "[providers.slow]\nbase_url = \"{base}/v1\"\nmax_retries = 0\nstream_idle_timeout_secs = 1\n"
        ),
        &[],
    );
    let mut route = env.route("slow", "m").await;
    fast(&mut route);
    route
}

#[tokio::test]
async fn idle_stream_times_out() {
    // Silence after the stream started: the stream ends with an error stop.
    let route = idle_route(&stalling_server(true).await).await;
    let mut stream = caller().send(&route, &request("x"), None).await.unwrap();
    let events = stream.collect().await;
    assert_eq!(events[0], StreamEvent::TextDelta { text: "hi".into() });
    assert_eq!(
        events.last(),
        Some(&StreamEvent::Stop {
            stop_reason: StopReason::Error
        })
    );
    assert_eq!(stream.failure().unwrap().class, ErrorClass::Timeout);
    // Silence before any event: a classified failure the caller can retry.
    let route = idle_route(&stalling_server(false).await).await;
    let err = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Timeout);
}

/// A route of provider `slow` at `base` with the default idle timeout
/// (120 s) unless `idle` sets one.
async fn default_idle_route(base: &str, idle: &str) -> Route {
    let env = Env::new(
        &format!("[providers.slow]\nbase_url = \"{base}/v1\"\nmax_retries = 0\n{idle}"),
        &[],
    );
    let mut route = env.route("slow", "m").await;
    fast(&mut route);
    route
}

fn capped(explicit: &[&str]) -> CallContext {
    let mut ctx = CallContext::new(SessionId::new(), "main");
    ctx.stale_cap = Some(StaleCap {
        cap: Duration::from_secs(1),
        explicit: explicit.iter().map(|p| (*p).to_owned()).collect(),
    });
    ctx
}

#[tokio::test]
async fn a_stale_cap_lowers_only_implicit_idle_timeouts() {
    // The default 120 s idle timeout is capped to 1 s.
    let route = default_idle_route(&stalling_server(false).await, "").await;
    let started = std::time::Instant::now();
    let err = caller()
        .send(&route, &request("x"), Some(&capped(&[])))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Timeout);
    assert!(started.elapsed() < Duration::from_secs(10));
    // A configured idle timeout is never overridden.
    let route = default_idle_route(&stalling_server(false).await, "").await;
    let ctx = capped(&["slow"]);
    let (sender, body) = (caller(), request("x"));
    let call = sender.send(&route, &body, Some(&ctx));
    assert!(
        tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .is_err(),
        "explicit idle timeout kept"
    );
    // The cap never raises a shorter timeout.
    let route = idle_route(&stalling_server(false).await).await;
    let mut ctx = capped(&[]);
    ctx.stale_cap.as_mut().unwrap().cap = Duration::from_mins(1);
    let started = std::time::Instant::now();
    let err = caller()
        .send(&route, &request("x"), Some(&ctx))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Timeout);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn idle_timeout_defaults_to_120_seconds() {
    let config = pmt_config::schema::ProviderConfig::default();
    assert_eq!(config.stream_idle_timeout_secs, 120);
    assert_eq!(config.request_timeout_secs, 600);
    assert_eq!(config.max_retries, 2);
}
