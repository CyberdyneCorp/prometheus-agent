//! Session start against a local server (spec: Capability-aware route
//! validation and minimum context window, "Small local window refused").

use jiff::Timestamp;
use pmt_providers::capability;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::Env;

#[tokio::test]
async fn small_local_window_reported_by_the_server_is_refused_before_any_model_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": "qwen3", "object": "model", "max_model_len": 32768}]
        })))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!("[providers.vllm]\nbase_url = \"{}/v1\"\n", server.uri()),
        &[],
    );
    let route = env.route("vllm", "qwen3").await;
    let provider = env.registry.lookup("vllm").unwrap();
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    env.catalog
        .refresh(&client, provider, None, Timestamp::now())
        .await
        .unwrap();
    let (meta, _) = env.catalog.metadata(&route.provider, &route.model);
    let err =
        capability::check_min_context(&route.label(), &meta, env.config.model.min_context_window)
            .unwrap_err();
    assert_eq!(err.code().code, "model.context_too_small");
    assert_eq!(err.exit_code(), 78);
    assert!(err.message().contains("32768"));
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.method == wiremock::http::Method::GET)
    );
}
