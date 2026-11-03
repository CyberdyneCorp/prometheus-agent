//! Model-provider plugins in sessions (spec: plugin-system, Model-provider
//! plugins): a registered provider joins the session's registry, its
//! request hooks run in the plugin and the request stays in the host.

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
    let out = tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
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
