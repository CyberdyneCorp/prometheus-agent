//! Real Hermes plugins, copied unchanged from hermes-agent, driven end to
//! end through the binary (spec: python-bridge, Model-provider plugins
//! through the bridge; Migration of Hermes plugins). Needs uv on PATH (see
//! `pmt_pybridge::testkit`).

mod common;

use std::path::Path;

use std::process::Stdio;

use self::common::{command, mock_home, ok, run, stderr, stdout};

/// Places fixture `name` as plugin `id`, adopts it and enables it with
/// `grant`.
fn install(home: &Path, name: &str, id: &str, grant: &str) {
    let dir = pmt_pybridge::testkit::place(home, name, id);
    ok(&run(
        home,
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", grant],
    ));
    ok(&run(home, &["plugins", "enable", id, "--grant", grant]));
}

const STREAM: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"from nim\"}}]}\n\ndata: [DONE]\n\n";

#[tokio::test(flavor = "multi_thread")]
async fn the_hermes_nvidia_provider_plugin_serves_a_session() {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // The plugin replaces the built-in `nvidia` under its own aliases, and
    // the profile's `default_max_tokens` reaches the request the host sends.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer nv-test"))
        .and(body_partial_json(serde_json::json!({
            "model": "nvidia/llama-3.3-70b-instruct",
            "max_tokens": 16384,
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    common::run_with_stdin(
        home.path(),
        &["secrets", "set", "NVIDIA_API_KEY"],
        "nv-test\n",
    );
    install(home.path(), "nvidia", "nvidia", "all");
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.nvidia]\nbase_url = \"{}/v1\"\nkey_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(listing.contains("nvidia  1.0.0  python"), "{listing}");
    assert!(!listing.contains("error"), "{listing}");
    let usage = home.path().join("usage.json");
    let (home_path, usage_path) = (home.path().to_path_buf(), usage.clone());
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &[
                "-z",
                "hi",
                "--provider",
                "nim",
                "--model",
                "nvidia/llama-3.3-70b-instruct",
                "--usage-file",
                usage_path.to_str().unwrap(),
            ],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "from nim\n");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&usage).unwrap()).unwrap();
    assert_eq!(report["billing_source"], "api_key:nvidia", "{report}");
}

#[test]
fn a_memory_providers_command_needs_it_to_be_the_active_provider() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "memcli", "memcli", "all");
    // Discovered from cli.py, but every profile runs the built-in store.
    let listing = ok(&run(home.path(), &["plugins", "run", "memcli"]));
    assert_eq!(listing, "plugin 'memcli' registers no commands\n");
    let out = run(
        home.path(),
        &["plugins", "run", "memcli", "memcli", "status"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(
            "command 'memcli' of memory provider 'memcli' is available only while 'memcli' is the active memory.provider (now 'builtin')"
        ),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_hermes_security_guidance_plugin_warns_the_model_after_a_dangerous_write() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "security-guidance", "security-guidance", "all");
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "load.py", content = "import pickle\nmodel = pickle.load(open('m.pkl', 'rb'))\n" }

[[rule]]
tool_result = "Security guidance"
text = "warned"
"#,
    )
    .unwrap();
    let out = command(home.path())
        .current_dir(&work)
        .args(["-z", "go", "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "warned\n", "{}", stderr(&out));
    // The write happened; the plugin only appended its guidance.
    assert!(
        std::fs::read_to_string(work.join("load.py"))
            .unwrap()
            .contains("pickle.load")
    );
    let results: Vec<String> = common::active_messages(home.path())
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        results
            .iter()
            .any(|r| r.contains("pickle") && r.contains("Pattern matches can be false positives")),
        "{results:?}"
    );
}

#[test]
fn the_hermes_disk_cleanup_plugin_is_refused_by_its_internal_imports() {
    let home = mock_home();
    let dir = pmt_pybridge::testkit::fixture("disk-cleanup");
    let out = run(home.path(), &["plugins", "validate", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(
        report.starts_with("disk-cleanup: incompatible\n"),
        "{report}"
    );
    assert!(
        report.contains(
            "hermes_constants at disk_cleanup.py:20 (Hermes internal import; runs while loading)"
        ),
        "{report}"
    );
}

#[test]
fn doctor_reports_the_bridge_network_and_limit_enforcement() {
    let home = mock_home();
    let before = stdout(&run(home.path(), &["doctor", "plugins"]));
    assert!(!before.contains("python bridge"), "{before}");
    let dir = pmt_pybridge::testkit::place(home.path(), "security-guidance", "security-guidance");
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    let doc: serde_json::Value =
        serde_json::from_str(&stdout(&run(home.path(), &["--json", "doctor", "plugins"]))).unwrap();
    let text = doc.to_string();
    assert!(
        text.contains("python bridge network: egress proxy for granted hosts; direct sockets"),
        "{doc}"
    );
    assert!(text.contains("python bridge resource limits: "), "{doc}");
}
