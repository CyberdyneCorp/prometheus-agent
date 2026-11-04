//! Python plugins in sessions and on the command line (spec:
//! python-bridge): the managed runtime, sandboxed sidecars, crash
//! isolation, model providers, CLI commands and host-bound APIs.
//! Needs uv on PATH (see `pmt_pybridge::testkit`).

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

fn chat(home: &Path, script: &str) -> std::process::Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .args(["-z", "go", "--mock-script", path.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn tool_results(home: &Path) -> Vec<String> {
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_hermes_tool_plugin_works_unchanged() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "spotify", "spotify", "all");
    let out = chat(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "spotify_play"
arguments = { track = "Blue in Green" }

[[rule]]
tool_result = "Blue in Green"
text = "now playing"
"#,
    );
    assert_eq!(stdout(&out), "now playing\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("\"playing\": \"Blue in Green\"")),
        "{results:?}"
    );
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("spotify  1.0.0  python  user  loaded"),
        "{listing}"
    );
}

#[test]
fn offline_first_use_fails_clearly_and_the_session_starts() {
    let home = mock_home();
    install(home.path(), "spotify", "spotify", "all");
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains(
            "Python runtime 3.12 is not installed and could not be downloaded; run prometheus runtimes install python@3.12"
        ),
        "{listing}"
    );
    let out = run(home.path(), &["-z", "hi"]);
    assert_eq!(stdout(&out), "mock: hi\n", "{}", stderr(&out));
}

#[test]
fn a_crashing_plugin_does_not_affect_the_agent() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "echo", "echo", "all");
    install(home.path(), "spotify", "spotify", "all");
    let out = chat(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "crash"
arguments = {}

[[rule]]
tool_result = "bridge_crashed"
[[rule.tool_calls]]
name = "spotify_play"
arguments = { track = "So What" }

[[rule]]
tool_result = "So What"
text = "survived"
"#,
    );
    assert_eq!(stdout(&out), "survived\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("bridge_crashed: plugin 'echo' exited (signal 6 (SIGABRT))")),
        "{results:?}"
    );
}

#[test]
fn a_plugin_cannot_read_the_users_ssh_keys() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let key = common::user_home(home.path()).join(".ssh/id_ed25519");
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::write(&key, "PRIVATE KEY").unwrap();
    install(home.path(), "echo", "echo", "all");
    let script = format!(
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "read"
arguments = {{ path = "{}" }}

[[rule]]
tool_result = "Error"
text = "done"
"#,
        key.display()
    );
    let out = chat(home.path(), &script);
    assert_eq!(stdout(&out), "done\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results.iter().any(|r| r.contains("PermissionError")),
        "{results:?}"
    );
    assert!(
        results.iter().all(|r| !r.contains("PRIVATE KEY")),
        "{results:?}"
    );
}

#[test]
fn an_argparse_error_maps_to_the_usage_exit() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "honcho", "honcho", "all");
    let out = run(
        home.path(),
        &["plugins", "run", "honcho", "honcho", "--bogus"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("usage: prometheus plugins run honcho honcho"),
        "{}",
        stderr(&out)
    );
    let out = run(
        home.path(),
        &["plugins", "run", "honcho", "honcho", "status"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "honcho status: ok\n");
}

#[test]
fn an_ungranted_injection_returns_false_and_the_log_names_the_capability() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "echo", "echo", "secrets.read:SPOTIFY_TOKEN");
    let out = chat(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "inject"
arguments = { content = "hi", session_key = "s1" }

[[rule]]
tool_result = "false"
text = "refused"
"#,
    );
    assert_eq!(stdout(&out), "refused\n", "{}", stderr(&out));
    let log =
        std::fs::read_to_string(home.path().join("profiles/default/logs/bridge/echo.log")).unwrap();
    assert!(log.contains("capability_denied: inject_messages"), "{log}");
}

#[test]
fn validate_reports_an_unsupported_call_with_its_file_and_line() {
    let home = mock_home();
    let dir = pmt_pybridge::testkit::fixture("echo");
    let out = run(home.path(), &["plugins", "validate", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(report.starts_with("echo: degraded\n"), "{report}");
    let line = report
        .lines()
        .find(|l| l.contains("register_telegram_handler"))
        .unwrap();
    assert!(line.contains("at __init__.py:"), "{report}");
    let incompatible = pmt_pybridge::testkit::fixture("platform");
    let out = run(
        home.path(),
        &["plugins", "validate", incompatible.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("register_platform at __init__.py:2"),
        "{}",
        stdout(&out)
    );
}

const STREAM: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"from copilot\"}}]}\n\ndata: [DONE]\n\n";

/// A profile using the Copilot-shaped Hermes model-provider plugin
/// against `server`.
fn copilot_home(server: &str) -> tempfile::TempDir {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    for (name, value) in [
        ("COPILOT_API_KEY", "gh-test".to_owned()),
        ("COPILOT_BASE_URL", format!("{server}/v1")),
    ] {
        common::run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            &format!("{value}\n"),
        );
    }
    install(home.path(), "copilot", "copilot", "all");
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    home
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copilot_provider_plugin_works_unchanged() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("editor-version", "prometheus/1.0"))
        .and(header("authorization", "Bearer gh-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = copilot_home(&server.uri());
    let usage = home.path().join("usage.json");
    let (home_path, usage_path) = (home.path().to_path_buf(), usage.clone());
    let out = tokio::task::spawn_blocking(move || {
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
    assert_eq!(report["billing_source"], "api_key:copilot", "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sidecar_crash_fails_the_call_and_the_fallback_chain_answers() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let home = copilot_home(&server.uri());
    common::append_config(
        home.path(),
        "\n[[fallback.chain]]\nprovider = \"mock\"\nmodel = \"mock\"\n",
    );
    let data = home.path().join("profiles/default/plugin-data/copilot");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("crash_headers"), "").unwrap();
    let home_path = home.path().to_path_buf();
    let out = tokio::task::spawn_blocking(move || {
        run(
            &home_path,
            &["-z", "hi", "--provider", "copilot", "--model", "gpt-4.1"],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "mock: hi\n", "{}", stderr(&out));
}

#[test]
fn a_profile_naming_a_missing_bridged_provider_fails_closed() {
    let home = mock_home();
    // The plugin is installed but the interpreter is not: its provider is
    // not registered, so the route is refused rather than guessed.
    install(home.path(), "copilot", "copilot", "all");
    let out = run(
        home.path(),
        &["-z", "hi", "--provider", "copilot", "--model", "gpt-4.1"],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(78), "{err}");
    assert!(
        err.contains("hint: prometheus plugins install github-copilot"),
        "{err}"
    );
    let out = run(
        home.path(),
        &[
            "--json",
            "-z",
            "hi",
            "--provider",
            "copilot",
            "--model",
            "gpt-4.1",
        ],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_str(stderr(&out).trim()).unwrap();
    assert_eq!(doc["error"]["code"], "provider.plugin_unavailable", "{doc}");
    assert_eq!(
        doc["error"]["hint"], "prometheus plugins install github-copilot",
        "{doc}"
    );
}

#[test]
fn a_skill_script_gets_its_environment_and_no_plugin_environment() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let index = home.path().join("index");
    pmt_pybridge::testkit::index(
        &index,
        &[
            (
                "pdfkit",
                "1.0.0",
                &[("pdfkit/__init__.py", "VERSION = '1.0.0'\n")],
            ),
            (
                "notes-sdk",
                "1.0.0",
                &[("notes_sdk/__init__.py", "VERSION = '1.0.0'\n")],
            ),
        ],
    );
    let served = pmt_pybridge::testkit::serve(&index);
    // Plugin `notes` has its own environment with notes-sdk.
    let dir = pmt_pybridge::testkit::place(home.path(), "lazy", "notes");
    std::fs::write(dir.join("requirements.txt"), "notes-sdk==1.0.0\n").unwrap();
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    let out = command(home.path())
        .args(["plugins", "update", "notes"])
        .env("UV_DEFAULT_INDEX", &served.url)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("built the python environment of 'notes'"),
        "{}",
        stderr(&out)
    );
    // Skill `pdf-tools` declares its own requirements.
    let skill = home.path().join("profiles/default/skills/pdf-tools");
    std::fs::create_dir_all(skill.join("scripts")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: pdf-tools\ndescription: Extract text from PDFs\nmetadata:\n  prometheus:\n    python:\n      requires_python: \">=3.11\"\n      requirements:\n        - pdfkit>=1,<2\n---\nRun the extractor.\n",
    )
    .unwrap();
    std::fs::write(
        skill.join("scripts/extract.py"),
        "import sys\nimport pdfkit\nprint('pdfkit', pdfkit.VERSION)\ntry:\n    import notes_sdk\n    print('notes_sdk visible')\nexcept ImportError:\n    print('notes_sdk hidden')\nprint('managed', 'runtimes/python' in sys.base_prefix or 'pybridge-python' in sys.base_prefix)\n",
    )
    .unwrap();
    let script_path = home.path().join("script.toml");
    std::fs::write(
        &script_path,
        format!(
            r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "python {}/scripts/extract.py" }}

[[rule]]
tool_result = "pdfkit"
text = "extracted"
"#,
            skill.display()
        ),
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", script_path.to_str().unwrap()])
        .env("UV_DEFAULT_INDEX", &served.url)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        stdout(&out),
        "extracted\n",
        "{} {:?}",
        stderr(&out),
        tool_results(home.path())
    );
    let results = tool_results(home.path()).join("\n");
    assert!(results.contains("pdfkit 1.0.0"), "{results}");
    assert!(results.contains("notes_sdk hidden"), "{results}");
    assert!(results.contains("managed True"), "{results}");
}

#[test]
fn a_shared_process_runs_one_sidecar_per_profile() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    ok(&run(home.path(), &["profile", "create", "b"]));
    for (profile, token) in [("default", "token-a"), ("b", "token-b")] {
        let dir = home
            .path()
            .join("profiles")
            .join(profile)
            .join("plugins/echo");
        let source = pmt_pybridge::testkit::fixture("echo");
        let status = std::process::Command::new("cp")
            .arg("-R")
            .arg(&source)
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        let p = ["-p", profile];
        ok(&run(
            home.path(),
            &[
                &p[..],
                &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
            ]
            .concat(),
        ));
        ok(&run(
            home.path(),
            &[&p[..], &["plugins", "enable", "echo", "--grant", "all"]].concat(),
        ));
        common::run_with_stdin(
            home.path(),
            &[&p[..], &["secrets", "set", "SPOTIFY_TOKEN"]].concat(),
            &format!("{token}\n"),
        );
    }
    // One process (as one gateway would) serving both profiles.
    let root = pmt_config::Home::open(home.path()).unwrap();
    let mut seen = Vec::new();
    for profile in ["default", "b"] {
        let dir = root
            .profile(&pmt_config::ProfileName::parse(profile).unwrap())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            dir,
            &pmt_config::Overrides::default(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        let _plugins = prometheus::plugins_host::for_session(&scope, home.path());
        let running = prometheus::plugins_host::bridge::running(&scope);
        assert_eq!(running.len(), 1);
        let answer = running[0]
            .tool("whoami", &serde_json::json!({}), &serde_json::json!({}))
            .unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        seen.push((
            running[0].status().pid.unwrap(),
            answer["home"].clone(),
            answer["spotify"].clone(),
            answer["profile"].clone(),
        ));
    }
    assert_ne!(seen[0].0, seen[1].0, "two sidecar processes");
    assert_ne!(seen[0].1, seen[1].1, "two data directories");
    assert!(
        seen[0]
            .1
            .as_str()
            .unwrap()
            .ends_with("profiles/default/plugin-data/echo"),
        "{seen:?}"
    );
    assert_eq!(
        (seen[0].2.as_str(), seen[1].2.as_str()),
        (Some("token-a"), Some("token-b"))
    );
    assert_eq!(
        (seen[0].3.as_str(), seen[1].3.as_str()),
        (Some("default"), Some("b"))
    );
    prometheus::plugins_host::bridge::shutdown_all();
}

/// A one-shot session on the web toolset with `web.search_backend =
/// backend`, whose model calls `web_search` and answers once the result
/// contains `expected`.
fn web_search_session(home: &Path, backend: &str, expected: &str) -> std::process::Output {
    common::append_config(home, &format!("\n[web]\nsearch_backend = \"{backend}\"\n"));
    let script = home.join("web.toml");
    std::fs::write(
        &script,
        format!(
            r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "web_search"
arguments = {{ query = "modal jazz", limit = 2 }}

[[rule]]
tool_result = "{expected}"
text = "searched"
"#
        ),
    )
    .unwrap();
    command(home)
        .args(["-z", "go", "-t", "web", "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_bridged_web_search_provider_serves_web_search_in_a_session() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "echo", "echo", "all");
    let out = web_search_session(home.path(), "echo-search", "modal jazz 1");
    assert_eq!(stdout(&out), "searched\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("modal jazz 0") && r.contains("https://example.com/1")),
        "{results:?}"
    );
}

#[test]
fn an_unregistered_web_search_provider_fails_closed_in_a_session() {
    let home = mock_home();
    let out = web_search_session(home.path(), "nowhere", "not installed");
    assert_eq!(stdout(&out), "searched\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("web_search provider 'nowhere' is not installed or not enabled")),
        "{results:?}"
    );
}

/// The sandbox bounds each sidecar process's data segment: one allocation
/// past `max_memory_bytes` is refused by the kernel at once, before any
/// sample could see it (its pages are never touched).
#[cfg(target_os = "linux")]
#[test]
fn the_os_refuses_an_allocation_past_the_memory_limit() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "echo", "echo", "all");
    common::append_config(
        home.path(),
        "\n[python_bridge]\nmax_memory_bytes = 268435456\n",
    );
    let out = chat(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "alloc"
arguments = { bytes = 536870912 }

[[rule]]
tool_result = "resource limit exceeded"
text = "bounded"
"#,
    );
    assert_eq!(stdout(&out), "bounded\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("resource limit exceeded (max_memory_bytes 256 MiB)")),
        "{results:?}"
    );
}
