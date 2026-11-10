//! Azure AI Foundry (spec: model-providers, Azure AI Foundry provider):
//! keyless Entra ID access, the wire picked from the deployment's
//! metadata, and the deployment list in the model picker.

use std::sync::Arc;

use pmt_config::schema::WireFormat;
use pmt_providers::StreamEvent;
use pmt_providers::auth::Accounts;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::mint::Minter;
use pmt_providers::azure::EntraSources;
use pmt_providers::detect::Detector;
use pmt_providers::metadata::Catalog;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, http};
use crate::support::{caller, sse};

fn config(server: &MockServer, extra: &str) -> String {
    format!(
        "[providers.azure-foundry]\nbase_url = \"{}/openai/v1\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n{extra}",
        server.uri()
    )
}

async fn deployments(server: &MockServer, auth: (&'static str, &'static str)) {
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(query_param("api-version", "2022-12-01"))
        .and(header(auth.0, auth.1))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "chat-prod", "model": "gpt-4o"},
            {"id": "sonnet", "model": "claude-sonnet-4-5"}
        ]})))
        .mount(server)
        .await;
}

fn accounts(setup: &Setup, entra: EntraSources) -> Arc<Accounts> {
    let accounts = Arc::try_unwrap(setup.accounts(Borrowed::none())).unwrap();
    Arc::new(
        accounts
            .with_detector(Detector::new(http()))
            .with_minter(Minter::new(http(), Some(setup.user_home())).with_entra(entra)),
    )
}

async fn route(setup: &Setup, accounts: &Accounts, model: &str) -> pmt_providers::Route {
    let resolver = pmt_providers::Resolver {
        registry: &setup.registry,
        config: &setup.config,
        secrets: &setup.keys,
        extras: accounts,
        probe: &pmt_providers::resolve::TcpProbe {
            timeout: std::time::Duration::from_millis(200),
        },
        ad_hoc: None,
    };
    let target = pmt_providers::resolve::Target {
        provider: "azure-foundry".to_owned(),
        model: Some(model.to_owned()),
        ..Default::default()
    };
    resolver
        .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
        .await
        .unwrap()
}

/// A managed identity endpoint answering `token`.
async fn managed_identity(server: &MockServer, token: &str) {
    Mock::given(method("GET"))
        .and(path("/metadata/identity/oauth2/token"))
        .and(header("metadata", "true"))
        .and(query_param("resource", "https://ai.azure.com"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": token, "expires_in": "3599"})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn keyless_entra_id_access_from_a_managed_identity() {
    let server = MockServer::start().await;
    managed_identity(&server, "entra-jwt-0123456789").await;
    deployments(&server, ("authorization", "Bearer entra-jwt-0123456789")).await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("authorization", "Bearer entra-jwt-0123456789"))
        .respond_with(sse("chat_tool_call.sse"))
        .mount(&server)
        .await;
    let setup = Setup::new(&config(&server, "auth = \"entra_id\"\n"), &[]);
    // No Azure CLI: the managed identity answers.
    let entra = EntraSources {
        az: Some(setup.root.join("no-az-here")),
        imds: Some(server.uri()),
    };
    let accounts = accounts(&setup, entra);
    let route = route(&setup, &accounts, "chat-prod").await;
    assert_eq!(route.wire_format, WireFormat::ChatCompletions);
    assert_eq!(route.billing_source(), "api_key:azure-foundry/minted");
    let events = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &crate::support::request("hi"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(events.last(), Some(StreamEvent::Stop { .. })));
    let sent = server.received_requests().await.unwrap();
    let call = sent
        .iter()
        .find(|r| r.url.path() == "/openai/v1/chat/completions")
        .unwrap();
    // The JWT goes only in the bearer header, never in `api-key`.
    assert!(call.headers.get("api-key").is_none());
}

/// The Azure CLI login (`az account get-access-token`) is the first
/// source; a fake `az` stands in for it.
#[cfg(unix)]
#[tokio::test]
async fn keyless_entra_id_access_from_the_azure_cli_login() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = MockServer::start().await;
    deployments(&server, ("authorization", "Bearer cli-jwt-0123456789")).await;
    let setup = Setup::new(&config(&server, "auth = \"entra_id\"\n"), &[]);
    let az = setup.root.join("az");
    std::fs::write(
        &az,
        "#!/bin/sh\n[ \"$1 $2 $4\" = \"account get-access-token https://ai.azure.com/.default\" ] || exit 2\n\
         echo '{\"accessToken\": \"cli-jwt-0123456789\", \"expires_on\": 32503680000}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&az, std::fs::Permissions::from_mode(0o755)).unwrap();
    let accounts = accounts(
        &setup,
        EntraSources {
            az: Some(az),
            imds: None,
        },
    );
    let route = route(&setup, &accounts, "chat-prod").await;
    assert_eq!(route.wire_format, WireFormat::ChatCompletions);
    assert_eq!(route.billing_source(), "api_key:azure-foundry/minted");
}

#[tokio::test]
async fn no_entra_source_fails_naming_both() {
    let server = MockServer::start().await;
    let setup = Setup::new(&config(&server, "auth = \"entra_id\"\n"), &[]);
    let accounts = accounts(
        &setup,
        EntraSources {
            az: Some(setup.root.join("no-az-here")),
            imds: None,
        },
    );
    let resolver = pmt_providers::Resolver {
        registry: &setup.registry,
        config: &setup.config,
        secrets: &setup.keys,
        extras: &*accounts,
        probe: &pmt_providers::resolve::TcpProbe {
            timeout: std::time::Duration::from_millis(200),
        },
        ad_hoc: None,
    };
    let target = pmt_providers::resolve::Target {
        provider: "azure-foundry".to_owned(),
        model: Some("chat-prod".to_owned()),
        ..Default::default()
    };
    let err = resolver
        .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert!(err.message().contains("Entra ID"), "{}", err.message());
    assert!(err.hint().unwrap().contains("az login"));
}

#[tokio::test]
async fn claude_deployment_uses_anthropic_wire() {
    let server = MockServer::start().await;
    deployments(&server, ("api-key", "foundry-key-0123456789")).await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .and(header("api-key", "foundry-key-0123456789"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    let setup = Setup::new(
        &config(&server, ""),
        &[("AZURE_FOUNDRY_API_KEY", "foundry-key-0123456789")],
    );
    let accounts = accounts(&setup, EntraSources::default());
    let claude = route(&setup, &accounts, "sonnet").await;
    assert_eq!(claude.wire_format, WireFormat::AnthropicMessages);
    assert_eq!(claude.base_url, format!("{}/anthropic", server.uri()));
    let events = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&claude, &crate::support::request("hi"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(events.last(), Some(StreamEvent::Stop { .. })));
    // Another deployment of the same resource stays on Chat Completions,
    // and the deployment list is read once.
    let gpt = route(&setup, &accounts, "chat-prod").await;
    assert_eq!(gpt.wire_format, WireFormat::ChatCompletions);
    let lists = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/openai/deployments")
        .count();
    assert_eq!(lists, 1);
    // The picker lists the deployments.
    let catalog = Catalog::new(&setup.config, None);
    let provider = setup.registry.lookup("azure-foundry").unwrap();
    assert!(
        catalog
            .refresh_route(&http(), provider, &gpt, jiff::Timestamp::now())
            .await
            .unwrap()
    );
    assert_eq!(catalog.picker_models(provider), vec!["chat-prod", "sonnet"]);
}
