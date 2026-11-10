//! Model calls in the metrics catalog (spec: observability, Metrics
//! catalog): every failed call counts in `prometheus.errors` by its kind
//! and in `prometheus.model.calls` with `outcome = error`; a finished one
//! counts its time to first token.

use pmt_obs::metrics::{self, global};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{Env, caller, request, sse};

fn config(uri: &str) -> String {
    format!("[providers.prim]\nbase_url = \"{uri}/prim/v1\"\nmax_retries = 0\n")
}

#[tokio::test]
async fn an_authentication_failure_counts_one_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/prim/v1/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), &[]);
    let route = env.route("prim", "m").await;
    let before = global()
        .snapshot()
        .value(metrics::ERRORS, &[("kind", "auth")]);
    let _ = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    let snapshot = global().snapshot();
    let after = snapshot.value(metrics::ERRORS, &[("kind", "auth")]);
    assert!(
        (after - before - 1.0).abs() < f64::EPSILON,
        "{before} -> {after}"
    );
    assert!(
        snapshot.value(
            metrics::MODEL_CALLS,
            &[("provider", "prim"), ("outcome", "error")]
        ) >= 1.0
    );
}

#[tokio::test]
async fn a_streamed_call_records_its_time_to_first_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/prim/v1/"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), &[]);
    let route = env.route("prim", "m").await;
    let _ = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        global()
            .snapshot()
            .value(metrics::MODEL_TTFT_MS, &[("provider", "prim")])
            >= 1.0
    );
}
