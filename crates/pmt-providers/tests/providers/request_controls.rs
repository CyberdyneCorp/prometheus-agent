//! Request controls against recorded HTTP fixtures (spec: model-providers,
//! Endpoint auto-detection for multi-region key providers; Session
//! affinity headers and per-model wire routing; OpenRouter provider
//! routing preferences; Fast mode service tier).

use std::time::Duration;

use pmt_config::schema::WireFormat;
use pmt_core::id::SessionId;
use pmt_providers::auxiliary::{AuxCaller, AuxRoute, AuxSource, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::detect::{Detector, ZAI_ENDPOINTS};
use pmt_providers::fast::Speed;
use pmt_providers::pool::{MemoryState, PoolState};
use pmt_providers::resolve::{CredentialExtras, Resolver, Target, TcpProbe, Trace};
use pmt_providers::transport::{self, Conversations};
use pmt_providers::upstream::PARETO_MODEL;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{Env, caller, request, sse};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

/// Pool state plus an endpoint detector probing `candidates`.
struct Detecting {
    state: MemoryState,
    detector: Detector,
}

impl Detecting {
    fn new(candidates: Vec<String>) -> Self {
        let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
        Self {
            state: MemoryState::default(),
            detector: Detector::new(client).with_candidates(candidates),
        }
    }
}

impl CredentialExtras for Detecting {
    fn pool_state(&self) -> Option<&dyn PoolState> {
        Some(&self.state)
    }

    fn detector(&self) -> Option<&Detector> {
        Some(&self.detector)
    }
}

/// The four Z.AI endpoints as paths on `server`, in priority order.
fn zai_candidates(server: &MockServer) -> Vec<String> {
    ["intl", "cn", "intl-coding", "cn-coding"]
        .iter()
        .map(|p| format!("{}/{p}/v4", server.uri()))
        .collect()
}

async fn zai_route(env: &Env, extras: &Detecting) -> pmt_providers::Route {
    let resolver = Resolver {
        registry: &env.registry,
        config: &env.config,
        secrets: &env.secrets,
        extras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let target = Target {
        provider: "zai".into(),
        model: Some("glm-5".into()),
        ..Target::default()
    };
    resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap()
}

async fn probes(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

/// Scenario: Coding-plan key lands on the coding endpoint.
#[tokio::test]
async fn a_coding_plan_key_lands_on_the_coding_endpoint_and_is_not_probed_again() {
    let server = MockServer::start().await;
    for (prefix, status) in [
        ("intl", 401),
        ("cn", 401),
        ("intl-coding", 200),
        ("cn-coding", 200),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/{prefix}/v4/chat/completions")))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({"choices": []})))
            .mount(&server)
            .await;
    }
    let env = Env::new(
        "[providers.zai]\nkey_hosts = [\"127.0.0.1\"]\n",
        &[("GLM_API_KEY", "glm-coding-plan-key")],
    );
    let extras = Detecting::new(zai_candidates(&server));
    let route = zai_route(&env, &extras).await;
    let coding = format!("{}/intl-coding/v4", server.uri());
    assert_eq!(route.base_url, coding);
    assert_eq!(probes(&server).await, 4, "one probe per endpoint");
    let sent = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["max_tokens"], 1);

    // The next session start reuses the remembered endpoint.
    let again = zai_route(&env, &extras).await;
    assert_eq!(again.base_url, coding);
    assert_eq!(probes(&server).await, 4, "no second probe set");
}

/// Scenario: Failed probes are not repeated immediately.
#[tokio::test]
async fn a_rejected_key_is_probed_once_within_the_ttl_and_uses_the_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let env = Env::new("", &[("GLM_API_KEY", "glm-rejected-key")]);
    let extras = Detecting::new(zai_candidates(&server));
    let first = zai_route(&env, &extras).await;
    let second = zai_route(&env, &extras).await;
    assert_eq!(first.base_url, ZAI_ENDPOINTS[0]);
    assert_eq!(second.base_url, ZAI_ENDPOINTS[0]);
    assert_eq!(probes(&server).await, 4, "the probe set ran once");
}

#[tokio::test]
async fn an_explicit_base_url_wins_over_detection_and_kimi_code_keys_need_no_probe() {
    let server = MockServer::start().await;
    let env = Env::new(
        "[providers.zai]\nbase_url = \"https://api.z.ai/api/coding/paas/v4\"\n",
        &[
            ("GLM_API_KEY", "glm-key"),
            ("KIMI_API_KEY", "sk-kimi-0123456789"),
        ],
    );
    let extras = Detecting::new(zai_candidates(&server));
    let zai = zai_route(&env, &extras).await;
    assert_eq!(zai.base_url, "https://api.z.ai/api/coding/paas/v4");
    assert_eq!(probes(&server).await, 0);

    let kimi = env.route("kimi-coding", "kimi-for-coding").await;
    assert_eq!(kimi.base_url, "https://api.kimi.com/coding");
    assert_eq!(kimi.wire_format, WireFormat::AnthropicMessages);
    let moonshot = Env::new("", &[("KIMI_API_KEY", "sk-moonshot-key")])
        .route("kimi-coding", "kimi-k2")
        .await;
    assert_eq!(moonshot.base_url, "https://api.moonshot.ai/v1");
    assert_eq!(moonshot.wire_format, WireFormat::ChatCompletions);
}

fn aux_route(route: &pmt_providers::Route) -> AuxRoute {
    AuxRoute {
        task: AuxTask::Title,
        route: route.clone(),
        source: AuxSource::Main,
        timeout: Duration::from_secs(10),
        retries: 0,
        extra_body: None,
        max_concurrency: None,
    }
}

fn affinity(received: &wiremock::Request) -> Option<String> {
    received
        .headers
        .get("x-opencode-session")
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Scenario: Affinity header stable within a conversation.
#[tokio::test]
async fn main_and_auxiliary_calls_of_a_conversation_share_one_affinity_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!(
            "[providers.opencode-zen]\nbase_url = \"{}/zen/v1\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
        &[("OPENCODE_ZEN_API_KEY", "zen-key-0123456789")],
    );
    let route = env.route("opencode-zen", "glm-5").await;
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    // One caller pair serves every session of the profile, as in the daemon.
    let conversations = Conversations::default();
    let main = caller().with_conversations(conversations.clone());
    let aux = AuxCaller::new(client.clone()).with_conversations(conversations.clone());
    let send = |session: SessionId| {
        let (main, aux, route) = (&main, &aux, route.clone());
        async move {
            let ctx = CallContext::new(session, "main");
            main.send(&route, &request("hi"), Some(&ctx))
                .await
                .unwrap()
                .collect()
                .await;
            aux.call(&aux_route(&route), &request("title?"), &ctx)
                .await
                .unwrap();
        }
    };
    let (one, two) = (SessionId::new(), SessionId::new());
    let _one = conversations.bind(one, "ses_root_one");
    let _two = conversations.bind(two, "ses_root_two");
    send(one).await;
    send(two).await;
    // The first session keeps its value after the second one opened.
    send(one).await;

    let received = server.received_requests().await.unwrap();
    let values: Vec<Option<String>> = received.iter().map(affinity).collect();
    assert_eq!(values.len(), 6);
    assert!(values[0].is_some());
    assert_eq!(values[0], values[1], "main and title call share the value");
    assert_eq!(values[2], values[3]);
    assert_ne!(values[0], values[2], "another conversation, another value");
    assert_eq!(values[4], values[0], "a session's value never changes");
    assert_eq!(values[5], values[0]);
    assert!(!values[0].as_deref().unwrap().contains("ses_root"));

    // Providers without the header never get it.
    let plain = Env::new(
        &format!(
            "[providers.relay]\nbase_url = \"{}/relay/v1\"\n",
            server.uri()
        ),
        &[],
    )
    .route("relay", "m")
    .await;
    assert!(plain.affinity_header.is_none());
}

/// Scenario: Per-model wire format on resume.
#[tokio::test]
async fn opencode_go_serves_minimax_over_anthropic_messages_on_every_resolution() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/go/v1/messages"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!(
            "[providers.opencode-go]\nbase_url = \"{}/go/v1\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
        &[("OPENCODE_GO_API_KEY", "go-key-0123456789")],
    );
    // A resumed session resolves its stored route again.
    let resumed = env.route("opencode-go", "MiniMax-M2.5").await;
    assert_eq!(resumed.wire_format, WireFormat::AnthropicMessages);
    caller()
        .send(&resumed, &request("again"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(crate::support::bodies(&server, "/messages").await.len(), 1);
    let qwen = env.route("opencode-go", "qwen3-coder").await;
    assert_eq!(qwen.wire_format, WireFormat::AnthropicMessages);
    let glm = env.route("opencode-go", "glm-5").await;
    assert_eq!(glm.wire_format, WireFormat::ChatCompletions);
    let (meta, _) = env.catalog.metadata("opencode-go", "kimi-k2-vision");
    assert!(meta.supports_vision);
}

/// Scenario: Routing ignored for other providers.
#[tokio::test]
async fn routing_preferences_are_sent_to_openrouter_only() {
    let env = Env::new(
        "[provider_routing]\nonly = [\"anthropic\"]\n",
        &[
            ("OPENROUTER_API_KEY", "or-key-0123456789"),
            ("ANTHROPIC_API_KEY", "ant-key-0123456789"),
        ],
    );
    let direct = env.route("anthropic", "claude-sonnet-4-5").await;
    assert!(
        transport::body(&direct, &request("hi"))
            .get("provider")
            .is_none()
    );
    let routed = env.route("openrouter", "anthropic/claude-sonnet-4.5").await;
    assert_eq!(
        transport::body(&routed, &request("hi"))["provider"],
        json!({"only": ["anthropic"]})
    );
    let nitro = env.route("openrouter", "meta/llama-4:nitro").await;
    let body = transport::body(&nitro, &request("hi"));
    assert_eq!(body["model"], "meta/llama-4");
    assert_eq!(body["provider"]["sort"], "throughput");
}

/// Scenario: Pareto score sent only for the router model.
#[tokio::test]
async fn the_pareto_block_rides_only_the_router_model() {
    let env = Env::new(
        "[openrouter]\nmin_coding_score = 0.8\n",
        &[("OPENROUTER_API_KEY", "or-key-0123456789")],
    );
    let pareto = env.route("openrouter", PARETO_MODEL).await;
    assert_eq!(
        transport::body(&pareto, &request("hi"))["plugins"],
        json!([{"id": "pareto-router", "min_coding_score": 0.8}])
    );
    let claude = env.route("openrouter", "anthropic/claude-x").await;
    assert!(
        transport::body(&claude, &request("hi"))
            .get("plugins")
            .is_none()
    );
}

#[tokio::test]
async fn a_fast_request_asks_the_priority_tier_only_where_the_route_has_one() {
    let env = Env::new(
        "",
        &[
            ("OPENAI_API_KEY", "sk-openai-0123456789"),
            ("ANTHROPIC_API_KEY", "ant-key-0123456789"),
            ("OPENROUTER_API_KEY", "or-key-0123456789"),
        ],
    );
    let mut fast = request("hi");
    fast.speed = Some(Speed::Fast);
    let openai = env.route("openai-api", "gpt-5").await;
    assert_eq!(transport::body(&openai, &fast)["service_tier"], "priority");
    assert!(
        transport::body(&openai, &request("hi"))
            .get("service_tier")
            .is_none()
    );
    let anthropic = env.route("anthropic", "claude-opus-4-6").await;
    assert_eq!(transport::body(&anthropic, &fast)["speed"], "fast");
    let headers = transport::route_headers(&anthropic, &fast, None);
    assert!(
        headers
            .iter()
            .any(|(n, v)| n == "anthropic-beta" && v.starts_with("fast-mode"))
    );
    let relay = env.route("openrouter", "openai/gpt-5").await;
    let body = transport::body(&relay, &fast);
    assert!(body.get("service_tier").is_none() && body.get("speed").is_none());
}
