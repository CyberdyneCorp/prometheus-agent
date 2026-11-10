//! The bundled first-party provider plugins (spec: model-providers,
//! Gemini native and AWS Bedrock providers): Gemini's native
//! `streamGenerateContent` and Bedrock's `SigV4`-signed `ConverseStream`
//! against recorded fixtures, the AWS credential chain, and Bedrock's
//! control-plane catalog.

use std::sync::Arc;

use pmt_config::schema::{ReasoningLevel, WireFormat};
use pmt_providers::StreamEvent;
use pmt_providers::auth::Accounts;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::mint::Minter;
use pmt_providers::aws::chain::Endpoints;
use pmt_providers::descriptor::BundledWire;
use pmt_providers::event::StopReason;
use pmt_providers::metadata::Catalog;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, http};
use crate::support::{Env, caller, fixture, request, sse};

/// The normalised tool call every recorded fixture of the suite makes.
fn expected_tool_call() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "toolu_01".into(),
            name: "read_file".into(),
        },
        StreamEvent::ToolCallEnd {
            index: 0,
            id: "toolu_01".into(),
            name: "read_file".into(),
            arguments: "{\"limit\":50,\"path\":\"src/main.rs\"}".into(),
        },
        StreamEvent::Stop {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

fn tool_events(events: &[StreamEvent]) -> Vec<StreamEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::ToolCallStart { .. }
                    | StreamEvent::ToolCallEnd { .. }
                    | StreamEvent::Stop { .. }
            )
        })
        .cloned()
        .collect()
}

fn usage(events: &[StreamEvent]) -> pmt_providers::Usage {
    events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage { usage } => Some(*usage),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn gemini_native_streams_a_tool_call_with_the_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "goog-key-0123456789"))
        .respond_with(sse("gemini_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!(
            "[providers.gemini]\nbase_url = \"{}/v1beta\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
        &[("GEMINI_API_KEY", "goog-key-0123456789")],
    );
    let route = env.route("gemini", "gemini-2.5-pro").await;
    assert_eq!(route.bundled, Some(BundledWire::GeminiNative));
    let mut req = request("read main");
    req.reasoning = Some(ReasoningLevel::Medium);
    let events = caller()
        .send(&route, &req, None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(tool_events(&events), expected_tool_call());
    assert!(events.contains(&StreamEvent::ReasoningDelta {
        text: "The user wants the file.".into()
    }));
    let usage = usage(&events);
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens
        ),
        (120, 4096, 42)
    );
    let sent = &server.received_requests().await.unwrap()[0];
    // The key travels only in its header, never as a bearer token.
    assert!(sent.headers.get("authorization").is_none());
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        8_192
    );
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "You are a careful assistant.\n\nProject: demo."
    );
}

#[tokio::test]
async fn a_gemini_openai_endpoint_speaks_chat_completions_with_a_bearer_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/openai/chat/completions"))
        .and(header("authorization", "Bearer goog-key-0123456789"))
        .respond_with(sse("chat_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!(
            "[providers.gemini]\nbase_url = \"{}/v1beta/openai\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
        &[("GEMINI_API_KEY", "goog-key-0123456789")],
    );
    let route = env.route("gemini", "gemini-2.5-pro").await;
    assert_eq!(route.bundled, None);
    assert_eq!(route.wire_format, WireFormat::ChatCompletions);
    let events = caller()
        .send(&route, &request("read main"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(tool_events(&events), expected_tool_call());
}

/// An AWS event-stream frame of `event` with `payload`.
fn frame(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(u8::try_from(name.len()).unwrap());
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = 12 + headers.len() + payload.len() + 4;
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
    out.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out.extend_from_slice(&headers);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out
}

/// The recorded `ConverseStream` events as an event-stream body.
fn converse_stream() -> ResponseTemplate {
    let events: Vec<(String, Value)> =
        serde_json::from_str(&fixture("bedrock_tool_call.events.json")).unwrap();
    let body: Vec<u8> = events
        .iter()
        .flat_map(|(event, payload)| frame(event, payload.to_string().as_bytes()))
        .collect();
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(body)
}

fn bedrock_config(server: &MockServer, extra: &str) -> String {
    format!(
        "[providers.bedrock]\nbase_url = \"{uri}\"\nmodels_url = \"{uri}/control\"\nregion = \"eu-west-1\"\n\
         key_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n{extra}",
        uri = server.uri()
    )
}

/// Accounts minting from the AWS chain with no instance metadata service
/// and the SSO portal on `server`.
fn aws_accounts(setup: &Setup, server: &MockServer) -> Arc<Accounts> {
    let accounts = Arc::try_unwrap(setup.accounts(Borrowed::none())).unwrap();
    let endpoints = Endpoints {
        imds: None,
        container_host: server.uri(),
        sso_portal: Some(format!("{}/sso", server.uri())),
    };
    Arc::new(
        accounts.with_minter(
            Minter::new(http(), Some(setup.user_home())).with_aws_endpoints(endpoints),
        ),
    )
}

#[tokio::test]
async fn bedrock_converse_stream_is_signed_and_normalised() {
    let server = MockServer::start().await;
    let model = "eu.anthropic.claude-sonnet-4-5-20250929-v1:0";
    Mock::given(method("POST"))
        .and(path(format!(
            "/model/{}/converse-stream",
            model.replace(':', "%3A")
        )))
        .respond_with(converse_stream())
        .mount(&server)
        .await;
    let setup = Setup::new(
        &bedrock_config(&server, ""),
        &[
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret-0123456789"),
            ("AWS_SESSION_TOKEN", "aws-session-0123456789"),
        ],
    );
    let accounts = aws_accounts(&setup, &server);
    let mut route = setup.route(&accounts, "bedrock").await.unwrap();
    model.clone_into(&mut route.model);
    assert_eq!(route.bundled, Some(BundledWire::BedrockConverse));
    assert_eq!(route.billing_source(), "api_key:bedrock/minted");
    let events = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("read main"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(tool_events(&events), expected_tool_call());
    assert_eq!(
        events[0],
        StreamEvent::ReasoningDelta {
            text: "The user wants the file.".into()
        }
    );
    let usage = usage(&events);
    assert_eq!((usage.input_tokens, usage.cache_read_tokens), (120, 4096));
    let sent = &server.received_requests().await.unwrap()[0];
    let auth = sent.headers["authorization"].to_str().unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            && auth.contains("/eu-west-1/bedrock/aws4_request"),
        "{auth}"
    );
    assert_eq!(
        sent.headers["x-amz-security-token"],
        "aws-session-0123456789"
    );
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["system"][0]["text"], "You are a careful assistant.");
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "read_file"
    );
}

#[tokio::test]
async fn no_aws_credentials_fail_naming_the_chain() {
    let server = MockServer::start().await;
    let setup = Setup::new(&bedrock_config(&server, ""), &[]);
    let accounts = aws_accounts(&setup, &server);
    let route = setup.route(&accounts, "bedrock").await.unwrap();
    let err = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("hi"), None)
        .await
        .unwrap_err();
    let error = err.to_error("bedrock");
    assert_eq!(error.code().code, "provider.missing_credential");
    assert!(
        error.message().contains("AWS credential chain"),
        "{}",
        error.message()
    );
    assert_eq!(error.exit_code(), 78);
    // Nothing was sent unsigned.
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// An `aws sso login` of profile `corp-dev` in `setup`'s user home.
fn sso_profile(setup: &Setup) {
    let aws = setup.user_home().join(".aws");
    std::fs::create_dir_all(aws.join("sso").join("cache")).unwrap();
    std::fs::write(
        aws.join("config"),
        "[profile corp-dev]\nsso_session = corp\nsso_account_id = 111122223333\n\
         sso_role_name = Developer\n\n[sso-session corp]\nsso_region = eu-west-1\n\
         sso_start_url = https://corp.awsapps.com/start\n",
    )
    .unwrap();
    // The AWS CLI names the cache file after the SHA-1 of the session.
    std::fs::write(
        aws.join("sso")
            .join("cache")
            .join("ee0bfd2552fbd840c02cc48b6e823320543c450f.json"),
        json!({"accessToken": "sso-access-token", "expiresAt": "2999-01-01T00:00:00Z"}).to_string(),
    )
    .unwrap();
}

async fn control_plane(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/sso/federation/credentials"))
        .and(query_param("role_name", "Developer"))
        .and(query_param("account_id", "111122223333"))
        .and(header("x-amz-sso_bearer_token", "sso-access-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"roleCredentials": {
                "accessKeyId": "ASIASSO", "secretAccessKey": "sso-secret",
                "sessionToken": "sso-session", "expiration": 32_503_680_000_000_i64
            }})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/control/foundation-models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"modelSummaries": [
            {"modelId": "anthropic.claude-sonnet-4-5-20250929-v1:0", "inputModalities": ["TEXT", "IMAGE"],
             "outputModalities": ["TEXT"], "responseStreamingSupported": true,
             "inferenceTypesSupported": ["INFERENCE_PROFILE"]},
            {"modelId": "amazon.nova-pro-v1:0", "inputModalities": ["TEXT", "IMAGE"],
             "outputModalities": ["TEXT"], "responseStreamingSupported": true,
             "inferenceTypesSupported": ["ON_DEMAND"]}
        ]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/control/inference-profiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries": [
            {"inferenceProfileId": "eu.anthropic.claude-sonnet-4-5-20250929-v1:0", "status": "ACTIVE",
             "type": "SYSTEM_DEFINED", "models": [{"modelArn":
             "arn:aws:bedrock:eu-west-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"}]}
        ]})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn bedrock_inference_profile_selected() {
    let server = MockServer::start().await;
    control_plane(&server).await;
    let setup = Setup::new(
        &bedrock_config(&server, "aws_profile = \"corp-dev\"\n"),
        &[],
    );
    sso_profile(&setup);
    let accounts = aws_accounts(&setup, &server);
    let route = setup.route(&accounts, "bedrock").await.unwrap();
    // `model list` mints the chain's credentials, then lists.
    let token = pmt_providers::resolve::CredentialExtras::mint(
        &*accounts,
        setup.registry.lookup("bedrock").unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let route = pmt_providers::Route {
        credential: route.credential.clone().with_value(token),
        ..route
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let catalog = Catalog::new(&setup.config, Some(dir.path().to_path_buf()));
    let provider = setup.registry.lookup("bedrock").unwrap();
    let now = jiff::Timestamp::now();
    assert!(
        catalog
            .refresh_route(&http(), provider, &route, now)
            .await
            .unwrap()
    );
    let ids = catalog.picker_models(provider);
    assert_eq!(
        ids,
        vec![
            "amazon.nova-pro-v1:0".to_owned(),
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0".to_owned()
        ]
    );
    let (profile, _) = catalog.metadata_at(
        "bedrock",
        "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
        provider.base_url(),
    );
    assert_eq!(profile.context_window, 200_000);
    assert_eq!(profile.context_source.as_str(), "listing");
    assert!(profile.supports_vision);
    let (nova, _) = catalog.metadata_at("bedrock", "amazon.nova-pro-v1:0", provider.base_url());
    assert_eq!(nova.context_window, 300_000);
    // The listing was signed with the SSO role credentials for the region.
    let listing = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/control/inference-profiles")
        .unwrap();
    let auth = listing.headers["authorization"].to_str().unwrap();
    assert!(
        auth.contains("Credential=ASIASSO/") && auth.contains("/eu-west-1/bedrock/"),
        "{auth}"
    );
    // A fresh listing is not fetched again within the TTL.
    assert!(
        !catalog
            .refresh_route(&http(), provider, &route, now)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_expired_sso_sign_in_asks_for_aws_sso_login() {
    let server = MockServer::start().await;
    let setup = Setup::new(
        &bedrock_config(&server, "aws_profile = \"corp-dev\"\n"),
        &[],
    );
    sso_profile(&setup);
    std::fs::write(
        setup
            .user_home()
            .join(".aws/sso/cache/ee0bfd2552fbd840c02cc48b6e823320543c450f.json"),
        json!({"accessToken": "old", "expiresAt": "2001-01-01T00:00:00Z"}).to_string(),
    )
    .unwrap();
    let accounts = aws_accounts(&setup, &server);
    let err = pmt_providers::resolve::CredentialExtras::mint(
        &*accounts,
        setup.registry.lookup("bedrock").unwrap(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert!(
        err.hint()
            .unwrap()
            .contains("aws sso login --profile corp-dev")
    );
}

#[tokio::test]
async fn container_credentials_are_a_link_of_the_chain() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/credentials/task"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "AccessKeyId": "ASIATASK", "SecretAccessKey": "task-secret",
            "Token": "task-token", "Expiration": "2999-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;
    let setup = Setup::new(
        &bedrock_config(&server, ""),
        &[(
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "/v2/credentials/task",
        )],
    );
    let accounts = aws_accounts(&setup, &server);
    let secret = pmt_providers::resolve::CredentialExtras::mint(
        &*accounts,
        setup.registry.lookup("bedrock").unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let keys = pmt_providers::aws::Keys::from_secret(&secret).unwrap();
    assert_eq!(keys.access_key_id, "ASIATASK");
}
