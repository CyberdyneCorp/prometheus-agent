//! Minted short-lived credentials (spec: model-providers, Minted
//! short-lived credentials): Vertex AI tokens from application default
//! credentials and service-account files, `key_cmd` helpers, the cache and
//! the re-mint after a mid-session 401.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pmt_providers::auth::Accounts;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::mint::Minter;
use pmt_providers::{Route, StreamEvent};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, http};
use crate::support::{caller, fixture, request, sse};

fn vertex_config(server: &MockServer) -> String {
    format!(
        "[providers.vertex]\nbase_url = \"{}/v\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n",
        server.uri()
    )
}

fn minting(setup: &Setup) -> Arc<Accounts> {
    let accounts = Arc::try_unwrap(setup.accounts(Borrowed::none())).unwrap();
    Arc::new(accounts.with_minter(Minter::new(http(), Some(setup.user_home()))))
}

/// The token endpoint answers `tok-1`, then `tok-2` for every later grant.
async fn token_endpoint(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "tok-1", "expires_in": 3600})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "tok-2", "expires_in": 3600})),
        )
        .with_priority(2)
        .mount(server)
        .await;
}

async fn grants(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/token")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

async fn route(setup: &Setup, accounts: &Accounts) -> Route {
    let mut route = setup.route(accounts, "vertex").await.unwrap();
    "gemini-2.5-pro".clone_into(&mut route.model);
    route
}

#[tokio::test]
async fn re_mint_on_mid_session_401() {
    let server = MockServer::start().await;
    token_endpoint(&server).await;
    Mock::given(method("POST"))
        .and(path("/v/chat/completions"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(sse("chat_text.sse"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // Revoked server-side after the first request.
    Mock::given(method("POST"))
        .and(path("/v/chat/completions"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(ResponseTemplate::new(401).set_body_string("token revoked"))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v/chat/completions"))
        .and(header("authorization", "Bearer tok-2"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let setup = Setup::new(&vertex_config(&server), &[]);
    let adc = setup.user_home().join(if cfg!(windows) {
        "AppData/Roaming/gcloud"
    } else {
        ".config/gcloud"
    });
    std::fs::create_dir_all(&adc).unwrap();
    std::fs::write(
        adc.join("application_default_credentials.json"),
        json!({
            "type": "authorized_user",
            "client_id": "cid.apps.googleusercontent.com",
            "client_secret": "csecret",
            "refresh_token": "1//refresh",
            "token_uri": format!("{}/token", server.uri()),
        })
        .to_string(),
    )
    .unwrap();
    let accounts = minting(&setup);
    let route = route(&setup, &accounts).await;
    assert_eq!(route.billing_source(), "api_key:vertex/minted");
    let caller = caller().with_credentials(Arc::clone(&accounts) as _);
    let req = request("x");
    let first = caller
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(first.last(), Some(StreamEvent::Stop { .. })));
    // The cached token is used until it is refused; then one re-mint and
    // one retry, and the turn continues.
    let second = caller
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(second.last(), Some(StreamEvent::Stop { .. })));
    let sent = grants(&server).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].contains("grant_type=refresh_token"));
    assert!(sent[0].contains("refresh_token=1%2F%2Frefresh"));
}

#[tokio::test]
async fn a_service_account_assertion_is_signed_with_its_key() {
    let server = MockServer::start().await;
    token_endpoint(&server).await;
    Mock::given(method("POST"))
        .and(path("/v/chat/completions"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let setup = Setup::new(&vertex_config(&server), &[]);
    let der = fixture("vertex_test_key.der.b64");
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{der}\n-----END PRIVATE KEY-----\n");
    let key_file = setup.root.join("sa.json");
    std::fs::write(
        &key_file,
        json!({
            "type": "service_account",
            "client_email": "agent@proj.iam.gserviceaccount.com",
            "private_key": pem,
            "token_uri": format!("{}/token", server.uri()),
        })
        .to_string(),
    )
    .unwrap();
    let mut setup = setup;
    setup.keys.insert(
        "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
        pmt_core::secret::SecretString::new(key_file.to_str().unwrap()),
    );
    let accounts = minting(&setup);
    let route = route(&setup, &accounts).await;
    caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap()
        .collect()
        .await;
    let body = grants(&server).await.remove(0);
    let assertion = body
        .split('&')
        .find_map(|kv| kv.strip_prefix("assertion="))
        .unwrap();
    let parts: Vec<&str> = assertion.split('.').collect();
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "agent@proj.iam.gserviceaccount.com");
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    let pair =
        ring::signature::RsaKeyPair::from_pkcs8(&STANDARD.decode(der.trim()).unwrap()).unwrap();
    let public = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        pair.public().as_ref().to_vec(),
    );
    public
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn ambiguous_helper_output_rejected() {
    let server = MockServer::start().await;
    let two_lines = if cfg!(windows) {
        "echo one& echo two"
    } else {
        "printf 'one\\ntwo\\n'"
    };
    let config = format!(
        "[providers.custom-mint]\nbase_url = \"{}/c/v1\"\nkey_cmd = \"{two_lines}\"\n",
        server.uri()
    );
    let setup = Setup::new(&config, &[]);
    let accounts = minting(&setup);
    let route = setup.route(&accounts, "custom-mint").await.unwrap();
    let refused = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    let error = refused.to_error("custom-mint");
    assert_eq!(error.code().code, "provider.missing_credential");
    assert!(
        error.message().contains("helper output must be one token"),
        "{}",
        error.message()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_helper_token_is_cached_and_sent_as_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/c/v1/chat/completions"))
        .and(header("authorization", "Bearer helper-token"))
        .respond_with(sse("chat_text.sse"))
        .expect(2)
        .mount(&server)
        .await;
    let marker = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let count = marker.path().join("runs");
    let shown = count.display().to_string().replace('\\', "/");
    let command = if cfg!(windows) {
        format!(
            "echo x>> \\\"{shown}\\\" & echo {{\\\"access_token\\\": \\\"helper-token\\\", \\\"expires_in\\\": 3600}}"
        )
    } else {
        format!(
            "echo x >> '{shown}'; echo '{{\\\"access_token\\\": \\\"helper-token\\\", \\\"expires_in\\\": 3600}}'"
        )
    };
    let config = format!(
        "[providers.custom-mint]\nbase_url = \"{}/c/v1\"\nkey_cmd = \"{command}\"\n",
        server.uri()
    );
    let setup = Setup::new(&config, &[]);
    let accounts = minting(&setup);
    let route = setup.route(&accounts, "custom-mint").await.unwrap();
    let caller = caller().with_credentials(Arc::clone(&accounts) as _);
    for _ in 0..2 {
        caller
            .send(&route, &request("x"), None)
            .await
            .unwrap()
            .collect()
            .await;
    }
    let runs = std::fs::read_to_string(&count).unwrap();
    assert_eq!(runs.lines().count(), 1, "the helper ran once: {runs:?}");
}
