//! GitHub Copilot through a bridged Hermes model-provider plugin, signed
//! in with GitHub's device code (spec: model-providers, Hermes
//! model-provider plugins hosted by the Python bridge): `auth add copilot`
//! keeps the GitHub token; each session exchanges it for a short-lived
//! Copilot API token, which is what reaches the Copilot API, and the call
//! is billed to the subscription sign-in. Fake GitHub and Copilot
//! endpoints on loopback; needs uv on PATH (see `pmt_pybridge::testkit`).

mod common;

use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use self::common::{mock_home, ok, run, stderr, stdout};

const STREAM: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"from copilot\"}}]}\n\ndata: [DONE]\n\n";

/// The fake GitHub device flow, token exchange and Copilot API.
async fn github(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .and(body_string_contains("client_id=Iv1.b507a08c87ecfe98"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1", "user_code": "WDJB-MJHT", "interval": 0, "expires_in": 900,
            "verification_uri": format!("{}/login/device", server.uri()),
        })))
        .mount(server)
        .await;
    // GitHub answers a pending poll with 200 and an `error`.
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"error": "authorization_pending"})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_github_token", "token_type": "bearer", "scope": "read:user",
        })))
        .mount(server)
        .await;
    let expires = jiff::Timestamp::now().as_second() + 1800;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/v2/token"))
        .and(header("authorization", "token gho_github_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "tid=copilot-short-lived", "expires_at": expires, "refresh_in": 1500,
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer tid=copilot-short-lived"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM),
        )
        .expect(1)
        .mount(server)
        .await;
}

/// Scenario: Copilot through the bridge.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_code_sign_in_reaches_the_bridged_copilot_with_a_short_lived_token() {
    let server = MockServer::start().await;
    github(&server).await;
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let uri = server.uri();
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.copilot.oauth]\ndevice_authorization_url = \"{uri}/login/device/code\"\n\
             token_url = \"{uri}/login/oauth/access_token\"\n\
             exchange_url = \"{uri}/copilot_internal/v2/token\"\nallowed_hosts = [\"127.0.0.1\"]\n"
        ),
    );
    common::run_with_stdin(
        home.path(),
        &["secrets", "set", "COPILOT_BASE_URL"],
        &format!("{uri}/v1\n"),
    );
    let dir = pmt_pybridge::testkit::place(home.path(), "copilot", "copilot");
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "copilot", "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let home_path = home.path().to_path_buf();
    let signed =
        pmt_core::spawn::blocking(move || run(&home_path, &["auth", "add", "copilot", "--device"]))
            .await
            .unwrap();
    assert!(signed.status.success(), "{}", stderr(&signed));
    assert!(stderr(&signed).contains("WDJB-MJHT"), "{}", stderr(&signed));
    let usage = home.path().join("usage.json");
    let (home_path, usage_path) = (home.path().to_path_buf(), usage.clone());
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &[
                "-z",
                "hi",
                "--provider",
                "copilot",
                "--model",
                "gpt-4.1",
                "--usage-file",
                usage_path.to_str().unwrap(),
            ],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "from copilot\n");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&usage).unwrap()).unwrap();
    assert_eq!(
        report["billing_source"], "subscription:copilot/default",
        "{report}"
    );
}
