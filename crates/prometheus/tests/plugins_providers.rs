//! Model-provider plugins in sessions and in the `model` and `auth`
//! commands (spec: plugin-system, Model-provider plugins): a registered
//! provider joins the registry, its request hooks (model listing
//! included) run in the plugin and the request stays in the host.

mod common;

use std::path::Path;

use self::common::{append_config, mock_home, ok, run, run_with_stdin, stderr, stdout};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn place(home: &Path, capabilities: &str) {
    let dir = home.join("profiles/default/plugins/llms");
    pmt_plugins::testkit::install("testing-provider", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"llms\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"provider@1.1\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    ok(&run(
        home,
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(home, &["plugins", "enable", "llms", "--grant", "all"]));
}

const STREAM: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"from the plugin provider\"}}]}\n\ndata: [DONE]\n\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_provider_serves_a_session_through_its_request_hooks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-plugin", "acme"))
        .and(header("authorization", "Bearer pk-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = mock_home();
    let base = format!("{}/v1", server.uri());
    place(home.path(), "providers = [\"acme\"]");
    append_config(
        home.path(),
        &format!("\n[plugins.entries.llms]\nmodel_ids = \"acme\"\nbase_url = \"{base}\"\n"),
    );
    run_with_stdin(
        home.path(),
        &["secrets", "set", "PLUGIN_LLM_KEY"],
        "pk-test\n",
    );
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let home_path = home.path().to_path_buf();
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &["-z", "hi", "--provider", "acme", "--model", "plugin-model"],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("from the plugin provider"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn model_and_auth_commands_see_plugin_providers_and_their_listing() {
    let home = mock_home();
    place(home.path(), "providers = [\"acme\"]");
    append_config(
        home.path(),
        "\n[plugins.entries.llms]\nmodel_ids = \"acme\"\nlisted = \"alpha,beta\"\n",
    );
    let listed = ok(&run(
        home.path(),
        &["--json", "model", "list", "--provider", "acme"],
    ));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let ids: Vec<&str> = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["alpha", "beta"]);
    let providers = ok(&run(
        home.path(),
        &["--json", "model", "list", "--providers"],
    ));
    assert!(
        providers.contains("\"id\":\"acme\"") && providers.contains("\"origin\":\"plugin\""),
        "{providers}"
    );
    // `auth` stores a key for the plugin provider like for a built-in.
    let out = run_with_stdin(home.path(), &["auth", "add", "acme"], "pk-test\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let status = ok(&run(home.path(), &["auth", "status", "acme"]));
    assert!(status.contains("acme"), "{status}");
    // A failing listing is reported and the catalog answers instead.
    ok(&run(
        home.path(),
        &["config", "set", "plugins.entries.llms.listed", "\"fail\""],
    ));
    let out = run(home.path(), &["model", "list", "--provider", "acme"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("acme: the plugin's model listing failed: listing failed on purpose"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_unregistered_plugin_provider_named_by_config_exits_78() {
    let home = mock_home();
    let out = run(
        home.path(),
        &["-z", "hi", "--provider", "acme", "--model", "plugin-model"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_plugin_claims_its_404_and_the_turn_falls_back() {
    // Spec scenario "Provider plugin claims its quirk": the classification
    // hook names the 404 `model_not_found` with `should_fallback`, so the
    // turn moves to the next route instead of failing (a 404 is a config
    // error, which never falls back on its own) and the route is not
    // retried.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string(
                r#"{"error":{"type":"not_found","message":"no such model here"}}"#,
            ),
        )
        .expect(2)
        .mount(&server)
        .await;
    let home = mock_home();
    let base = format!("{}/v1", server.uri());
    place(home.path(), "providers = [\"acme\"]");
    append_config(
        home.path(),
        &format!(
            "\n[plugins.entries.llms]\nmodel_ids = \"acme\"\nbase_url = \"{base}\"\n\n\
             [[fallback.chain]]\nprovider = \"mock\"\nmodel = \"mock\"\n"
        ),
    );
    run_with_stdin(
        home.path(),
        &["secrets", "set", "PLUGIN_LLM_KEY"],
        "pk-test\n",
    );
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let ask = |home: std::path::PathBuf| async move {
        pmt_core::spawn::blocking(move || {
            run(
                &home,
                &["-z", "hi", "--provider", "acme", "--model", "plugin-model"],
            )
        })
        .await
        .unwrap()
    };
    // Without the hook the 404 fails the turn.
    let out = ask(home.path().to_path_buf()).await;
    assert!(!out.status.success(), "{}", stdout(&out));
    let dir = home.path().join("profiles/default/plugins/quirks");
    pmt_plugins::testkit::install("testing-hooks", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"quirks\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"hook@1.2\"]\n\n[capabilities]\nhooks = [\"transform_api_error_classification\"]\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "quirks", "--grant", "all"],
    ));
    append_config(
        home.path(),
        "\n[plugins.entries.quirks]\nhooks = \"transform_api_error_classification\"\n\
         mode = \"classify:{'reason':'model_not_found','should_fallback':true}\"\n",
    );
    let out = ask(home.path().to_path_buf()).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "mock: hi\n");
}

/// Scenario: Plugin cannot read the PKCE verifier (spec: model-providers,
/// Plugin OAuth and external-process providers). The plugin declares a
/// PKCE sign-in; the host runs it against a mock identity provider and
/// signs requests itself, and the plugin's `prepare-request` (which copies
/// what it was given into `plugin_saw`) sees only the opaque credential
/// handle: no verifier, `state`, access or refresh token.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_sign_in_runs_in_the_host_and_the_plugin_gets_only_a_handle() {
    const ACCESS: &str = "plugin-access-token-0123456789";
    const REFRESH: &str = "plugin-refresh-token-0123456789";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": ACCESS, "refresh_token": REFRESH, "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {ACCESS}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = mock_home();
    place(home.path(), "providers = [\"acme\"]");
    append_config(
        home.path(),
        &format!(
            "\n[plugins.entries.llms]\nmodel_ids = \"acme\"\nbase_url = \"{0}/v1\"\noauth_issuer = \"{0}\"\n",
            server.uri()
        ),
    );
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let mut add = common::command(home.path());
    add.args(["auth", "add", "acme", "--type", "oauth"]);
    let (code, text) = pmt_core::spawn::blocking(move || common::browser_sign_in(&mut add))
        .await
        .unwrap();
    assert_eq!(code, Some(0), "{text}");
    let exchange = &server.received_requests().await.unwrap()[0];
    let form: std::collections::BTreeMap<String, String> =
        url::form_urlencoded::parse(&exchange.body)
            .into_owned()
            .collect();
    assert_eq!(form["client_id"], "testing-plugin");
    let verifier = form["code_verifier"].clone();
    let state = text
        .split("state=")
        .nth(1)
        .and_then(|rest| rest.split(['&', '\n']).next())
        .expect("the authorize URL's state")
        .to_owned();
    let home_path = home.path().to_path_buf();
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &["-z", "hi", "--provider", "acme", "--model", "plugin-model"],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("from the plugin provider"));
    let requests = server.received_requests().await.unwrap();
    let call = requests
        .iter()
        .find(|r| r.url.path() == "/v1/chat/completions")
        .expect("the model call");
    let sent: serde_json::Value = serde_json::from_slice(&call.body).unwrap();
    assert!(sent.get("prometheus_credential").is_none(), "{sent}");
    let saw = sent["plugin_saw"].as_str().expect("the plugin's copy");
    assert!(
        saw.contains(r#""prometheus_credential":"subscription:acme/"#),
        "{saw}"
    );
    for secret in [
        verifier.as_str(),
        state.as_str(),
        ACCESS,
        REFRESH,
        "browser-code",
    ] {
        assert!(!saw.contains(secret), "the plugin saw {secret}: {saw}");
    }
}
