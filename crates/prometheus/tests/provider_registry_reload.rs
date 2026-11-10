//! Provider registry changes apply at session boundaries (spec:
//! model-providers, Provider registry changes apply at session
//! boundaries): a running session keeps the provider descriptors it
//! started with until `/provider reload --now`.

mod common;

#[cfg(unix)]
mod line {
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use expectrl::session::OsSession;
    use expectrl::{Eof, Expect};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::common::{append_config, mock_home, ok, run, run_with_stdin, stdout};

    fn manifest(dir: &Path, version: &str) {
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "manifest_version = 1\nid = \"llms\"\nversion = \"{version}\"\nkind = \"wasm\"\n\
                 description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
                 worlds = [\"provider@1.1\"]\n\n[capabilities]\nproviders = [\"acme\"]\n"
            ),
        )
        .unwrap();
    }

    fn reply(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
            ))
    }

    fn spawn(home: &Path) -> OsSession {
        let mut cmd: Command = super::common::command(home);
        cmd.args([
            "--set",
            "ui.interface=line",
            "--provider",
            "acme",
            "--model",
            "plugin-model",
        ])
        .current_dir(home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR");
        let mut session = OsSession::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(Duration::from_secs(90)));
        session
    }

    /// A mock server with the two versions' endpoints and a home with
    /// provider plugin `llms` 1.0.0 serving `acme` from the first.
    fn setup() -> (
        tokio::runtime::Runtime,
        MockServer,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
            Mock::given(method("POST"))
                .and(path("/one/chat/completions"))
                .and(header("x-plugin", "acme"))
                .respond_with(reply("served by version one"))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/two/chat/completions"))
                .and(header("x-plugin", "acme"))
                .respond_with(reply("served by version two"))
                .mount(&server)
                .await;
        });
        let home = mock_home();
        let dir = home.path().join("profiles/default/plugins/llms");
        pmt_plugins::testkit::install("testing-provider", &dir);
        manifest(&dir, "1.0.0");
        ok(&run(
            home.path(),
            &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
        ));
        ok(&run(
            home.path(),
            &["plugins", "enable", "llms", "--grant", "all"],
        ));
        append_config(
            home.path(),
            &format!(
                "\n[plugins.entries.llms]\nmodel_ids = \"acme\"\nbase_url = \"{}/one\"\n",
                server.uri()
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

        (runtime, server, home, dir)
    }

    /// Re-records the plugin's install digest after an in-place upgrade.
    fn record(dir: &Path) {
        pmt_plugins::record::InstallRecord {
            source: pmt_plugins::record::Origin::Adopted,
            location: dir.to_string_lossy().into_owned(),
            r#ref: None,
            package_digest: None,
            digest: pmt_plugins::digest::of_dir(dir).unwrap(),
            signer: None,
            tier: None,
            installed_at: "2026-11-06T00:00:00Z".to_owned(),
            flags: pmt_plugins::record::Flags::default(),
            held_update: None,
        }
        .write(dir)
        .unwrap();
    }

    #[test]
    fn plugin_edited_while_a_session_runs() {
        let (_runtime, server, home, dir) = setup();
        let mut p = spawn(home.path());
        p.expect("› ").unwrap();
        p.send_line("first").unwrap();
        p.expect("served by version one").unwrap();
        p.expect("› ").unwrap();

        // The upgrade: version 1.1.0 serves from another endpoint.
        manifest(&dir, "1.1.0");
        record(&dir);
        ok(&run(
            home.path(),
            &[
                "config",
                "set",
                "plugins.entries.llms.base_url",
                &format!("{}/two", server.uri()),
            ],
        ));
        ok(&run(home.path(), &["plugins", "grants", "approve", "llms"]));
        p.send_line("/plugins reload llms").unwrap();
        p.expect("next session").unwrap();
        p.expect("› ").unwrap();
        p.send_line("second").unwrap();
        p.expect("served by version one").unwrap();
        p.expect("› ").unwrap();

        p.send_line("/provider reload").unwrap();
        p.expect("apply to new sessions").unwrap();
        p.expect("› ").unwrap();
        p.send_line("/provider reload --now").unwrap();
        p.expect("changed: acme").unwrap();
        p.expect("› ").unwrap();
        p.send_line("third").unwrap();
        p.expect("served by version two").unwrap();
        p.expect("› ").unwrap();
        p.send_line("/exit").unwrap();
        p.expect(Eof).unwrap();

        // A new session gets the new descriptor.
        let fresh = run(
            home.path(),
            &["-z", "hi", "--provider", "acme", "--model", "plugin-model"],
        );
        assert!(
            stdout(&fresh).contains("served by version two"),
            "{}\n{}",
            stdout(&fresh),
            super::common::stderr(&fresh)
        );

        let listed = ok(&run(home.path(), &["--json", "sessions", "list", "--all"]));
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        let session = listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .find(|id| {
                ok(&run(
                    home.path(),
                    &["trajectory", "show", id, "--format", "jsonl"],
                ))
                .lines()
                .any(|line| {
                    line.contains("\"type\":\"provider.registry_reloaded\"")
                        && line.contains("\"changed\":[\"acme\"]")
                })
            });
        assert!(
            session.is_some(),
            "the reload is recorded in the trajectory"
        );
    }
}
