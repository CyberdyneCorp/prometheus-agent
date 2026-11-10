//! Azure AI Foundry end to end (spec: model-providers, Azure AI Foundry
//! provider): a keyless session on the Azure CLI login, found on `PATH`.

mod common;

/// Scenario: Keyless Entra ID access, with a fake `az` on `PATH`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_session_uses_the_azure_cli_login_without_a_key() {
    use std::os::unix::fs::PermissionsExt as _;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(header("authorization", "Bearer cli-jwt-0123456789"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "chat-prod", "model": "gpt-4o"}
        ]})))
        .mount(&server)
        .await;
    let stream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello from azure\"}}]}\n\n\
                  data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n\
                  data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("authorization", "Bearer cli-jwt-0123456789"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream),
        )
        .mount(&server)
        .await;
    let home = common::temp_home();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let az = bin.join("az");
    std::fs::write(
        &az,
        "#!/bin/sh\necho '{\"accessToken\": \"cli-jwt-0123456789\", \"expires_on\": 32503680000}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&az, std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = home.path().join("profiles/default/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "[budgets]\nallow_unpriced = true\n[providers.azure-foundry]\nbase_url = \"{}/openai/v1\"\nauth = \"entra_id\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    )
    .unwrap();
    let path_var = format!("{}:/usr/bin:/bin", bin.display());
    let usage = home.path().join("usage.json");
    let out = common::command(home.path())
        .args([
            "-z",
            "hi",
            "--provider",
            "azure-foundry",
            "--model",
            "chat-prod",
            "--usage-file",
            usage.to_str().unwrap(),
        ])
        .env("PATH", path_var)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(common::ok(&out), "hello from azure\n");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(usage).unwrap()).unwrap();
    assert_eq!(report["billing_source"], "api_key:azure-foundry/minted");
}
