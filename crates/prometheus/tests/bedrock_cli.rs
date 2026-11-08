//! The bundled Bedrock provider end to end (spec: model-providers, Gemini
//! native and AWS Bedrock providers): the control-plane catalog in `model
//! list`, the missing-credential exit of a non-interactive session, and
//! the credential-chain and region diagnostics of `doctor`.

mod common;

use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{command, ok, stderr, temp_home};

fn write_config(home: &Path, text: &str) {
    let file = home.join("profiles/default/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

/// `args` with the AWS keys of the environment secrets.
fn run_with_keys(home: &Path, args: &[&str]) -> Output {
    command(home)
        .args(args)
        .env("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
        .env("AWS_SECRET_ACCESS_KEY", "aws-secret-0123456789")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Scenario: Bedrock inference profile selected (the listing half; the
/// SSO credentials half is in pmt-providers' `bundled` tests).
#[tokio::test(flavor = "multi_thread")]
async fn model_list_shows_the_regions_profiles_and_models_with_windows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/foundation-models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"modelSummaries": [
                {"modelId": "amazon.nova-pro-v1:0", "inputModalities": ["TEXT", "IMAGE"],
                 "responseStreamingSupported": true, "inferenceTypesSupported": ["ON_DEMAND"]}
            ]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/inference-profiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries": [
            {"inferenceProfileId": "eu.anthropic.claude-sonnet-4-5-20250929-v1:0", "status": "ACTIVE"}
        ]})))
        .mount(&server)
        .await;
    let home = temp_home();
    write_config(
        home.path(),
        &format!(
            "[providers.bedrock]\nregion = \"eu-west-1\"\nmodels_url = \"{}\"\n",
            server.uri()
        ),
    );
    let out = run_with_keys(
        home.path(),
        &["--json", "model", "list", "--provider", "bedrock"],
    );
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    let rows: Vec<(String, u64)> = doc["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["model"].as_str().unwrap().to_owned(),
                m["context_window"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("amazon.nova-pro-v1:0".to_owned(), 300_000),
            (
                "eu.anthropic.claude-sonnet-4-5-20250929-v1:0".to_owned(),
                200_000
            ),
        ]
    );
    let sent = server.received_requests().await.unwrap();
    let auth = sent[0].headers["authorization"].to_str().unwrap();
    assert!(auth.contains("/eu-west-1/bedrock/aws4_request"), "{auth}");
    // The listing (with its windows) is cached: a second listing asks
    // nothing.
    ok(&run_with_keys(
        home.path(),
        &["model", "list", "--provider", "bedrock"],
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), sent.len());
}

/// Scenario: No AWS credentials.
#[test]
fn a_session_without_aws_credentials_exits_78_naming_the_chain() {
    let home = temp_home();
    write_config(
        home.path(),
        "[budgets]\nallow_unpriced = true\n[providers.bedrock]\nmax_retries = 0\n",
    );
    let out = command(home.path())
        .args([
            "-z",
            "hi",
            "--provider",
            "bedrock",
            "--model",
            "amazon.nova-pro-v1:0",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("AWS credential chain"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn doctor_reports_the_region_and_the_credential_source() {
    let home = temp_home();
    write_config(
        home.path(),
        "[model]\nprovider = \"bedrock\"\ndefault = \"amazon.nova-pro-v1:0\"\n\
         [providers.bedrock]\nregion = \"eu-west-1\"\n",
    );
    let out = run_with_keys(home.path(), &["--json", "doctor", "providers"]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = |name: &str| {
        doc["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} check: {doc}"))
    };
    let region = check("aws_region");
    assert_eq!(region["status"], "ok", "{region}");
    assert!(region["message"].as_str().unwrap().contains("eu-west-1"));
    let credentials = check("aws_credentials");
    assert_eq!(credentials["status"], "ok", "{credentials}");
    assert!(
        credentials["message"]
            .as_str()
            .unwrap()
            .contains("environment secrets"),
        "{credentials}"
    );
    // A misspelled region fails; no credentials anywhere is a warning
    // offline (only --online asks the instance role).
    write_config(
        home.path(),
        "[model]\nprovider = \"bedrock\"\ndefault = \"amazon.nova-pro-v1:0\"\n\
         [providers.bedrock]\nregion = \"eu-west\"\n",
    );
    let out = command(home.path())
        .args(["--json", "doctor", "providers"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let statuses: Vec<(String, String)> = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"].as_str().unwrap().starts_with("aws_"))
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("aws_region".to_owned(), "fail".to_owned()),
            ("aws_credentials".to_owned(), "warn".to_owned()),
        ]
    );
}
