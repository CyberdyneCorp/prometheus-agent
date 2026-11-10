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
    // Plugin and sidecar isolation: the call fails without ambient
    // authority, naming the capability.
    assert!(
        log.contains("capability not granted: plugin.inject_messages"),
        "{log}"
    );
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

/// A Hermes model-provider plugin declaring its sign-in through the
/// documented `hermes_cli.auth_oauth_pkce_plugin` (spec: model-providers,
/// Plugin OAuth and external-process providers): the declaration becomes
/// the host's browser PKCE sign-in, and a broker that would receive the
/// verifier (Solstice's shape) is refused at load.
#[test]
fn a_declared_pkce_sign_in_becomes_the_hosts_and_a_broker_is_refused() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "pkce", "pkce", "all");
    let out = run(home.path(), &["--json", "auth", "status", "example-pkce"]);
    let doc: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["login_flows"], serde_json::json!(["browser"]), "{doc}");
    let out = run(home.path(), &["auth", "status", "brokered"]);
    assert!(!out.status.success(), "{}", stdout(&out));
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

/// A one-shot session on `toolset` whose model calls `tool` with
/// `arguments` (inline TOML) and answers once the result contains
/// `expected`.
fn media_session(
    home: &Path,
    toolset: &str,
    tool: &str,
    arguments: &str,
    expected: &str,
) -> std::process::Output {
    let script = home.join("media.toml");
    std::fs::write(
        &script,
        format!(
            r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "{tool}"
arguments = {arguments}

[[rule]]
tool_result = "{expected}"
text = "made"
"#
        ),
    )
    .unwrap();
    command(home)
        .args(["-z", "go", "-t", toolset, "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// plugin-system, Provider-object plugins fail closed when unregistered:
/// `image_gen.provider` and `tts.provider` naming a bridged plugin's
/// providers reach them, billed as the plugin reports.
#[test]
fn bridged_image_and_speech_providers_serve_their_tools_in_a_session() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "studio", "studio", "all");
    common::append_config(
        home.path(),
        "\n[image_gen]\nprovider = \"studio-pics\"\n\n[tts]\nprovider = \"studio-voice\"\n",
    );
    let out = media_session(
        home.path(),
        "image_gen",
        "image_generate",
        r#"{ prompt = "a fox", aspect_ratio = "square" }"#,
        "studio-pics",
    );
    assert_eq!(stdout(&out), "made\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    let image = results
        .iter()
        .find(|r| r.contains("studio-pics"))
        .unwrap_or_else(|| panic!("{results:?}"));
    assert!(image.starts_with("MEDIA:"), "{image}");
    assert!(image.contains("\"model\":\"pics-1\""), "{image}");
    assert!(image.contains("\"size\":\"1x1\""), "{image}");
    let out = media_session(
        home.path(),
        "tts",
        "text_to_speech",
        r#"{ text = "Hello there." }"#,
        "studio-voice",
    );
    assert_eq!(stdout(&out), "made\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    let speech = results
        .iter()
        .find(|r| r.contains("studio-voice"))
        .unwrap_or_else(|| panic!("{results:?}"));
    let path = speech
        .lines()
        .next()
        .unwrap()
        .strip_prefix("MEDIA:")
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"ID3Hello there.");
}

/// `stt.provider` naming a bridged plugin's transcription provider is the
/// one voice input uses.
#[test]
fn a_bridged_transcription_provider_transcribes_voice_input() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "studio", "studio", "all");
    common::append_config(home.path(), "\n[stt]\nprovider = \"studio-ears\"\n");
    let root = pmt_config::Home::open(home.path()).unwrap();
    let dir = root
        .profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    let plugins = prometheus::plugins_host::for_session(&scope, home.path());
    let mut setup = pmt_media::explain::inspection_setup(
        prometheus::chat::session::media::credentials(&scope),
        scope.dir(),
        None,
    )
    .unwrap();
    setup.plugins = prometheus::chat::session::media_plugins::PluginMedia::shared(
        plugins.current().providers.clone(),
        pmt_plugins::host::CallInfo {
            session_id: None,
            surface: "gateway".to_owned(),
            cwd: None,
        },
    );
    let clip = home.path().join("note.ogg");
    std::fs::write(&clip, b"OggS0123456789").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transcript = runtime
        .block_on(pmt_media::MediaSession::new(setup).transcribe(&clip))
        .unwrap();
    assert_eq!(transcript.text, "heard 14 bytes in en");
    assert_eq!(transcript.provider, "studio-ears");
    prometheus::plugins_host::bridge::shutdown_all();
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

/// A Chat Completions server for the restart test: each request's raw body
/// is sent to the test, `answer(n)` runs before the n-th reply (0-based) and
/// returns its SSE body.
fn chat_server(
    answer: impl Fn(usize) -> String + Send + 'static,
) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufReader, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for (n, mut stream) in listener.incoming().flatten().enumerate() {
            let body = common::read_http_request(&mut BufReader::new(stream.try_clone().unwrap()));
            let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            let sse = answer(n);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
        }
    });
    (format!("http://{addr}/v1"), rx)
}

const DONE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn tool_call_sse(name: &str) -> String {
    let call = serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": format!("call_{name}"), "type": "function",
        "function": {"name": name, "arguments": "{}"}}]}, "finish_reason": null}]});
    let end =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});
    format!("data: {call}\n\ndata: {end}\n\ndata: [DONE]\n\n")
}

/// Waits until the echo sidecar of `profile` runs after `restarts` restarts.
fn running_after(profile: &Path, restarts: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let rows = pmt_pybridge::supervisor::published(profile);
        if rows.iter().any(|r| {
            r.plugin == "echo"
                && r.restarts >= restarts
                && r.state == pmt_pybridge::supervisor::State::Running
        }) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The JSON value starting at `text[0]` (an object or array), as sent.
fn balanced(text: &str) -> &str {
    let (mut depth, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '{' | '[' if !quoted => depth += 1,
            '}' | ']' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return &text[..=i];
                }
            }
            _ => {}
        }
    }
    text
}

/// The raw bytes of a Chat Completions request's tools array and of its
/// first message (the system prompt): the prefix the provider caches.
fn cached_prefix(raw: &str) -> (String, String) {
    let tools = raw
        .find("\"tools\":[")
        .map_or("", |i| balanced(&raw[i + 8..]));
    let first = raw
        .find("\"messages\":[{")
        .map_or("", |i| balanced(&raw[i + 12..]));
    (tools.to_owned(), first.to_owned())
}

#[test]
fn a_sidecar_restart_does_not_mutate_a_live_sessions_tools_or_prompt() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "echo", "echo", "all");
    let profile = home.path().join("profiles/default");
    let data = profile.join("plugin-data/echo");
    let watched = profile.clone();
    // 1st reply: the sidecar will re-register without `whoami`, then `crash`
    // kills it. 2nd: once the restarted sidecar runs, call `whoami`. 3rd: done.
    let (url, bodies) = chat_server(move |n| match n {
        0 => {
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("drop_tool"), "whoami").unwrap();
            tool_call_sse("crash")
        }
        1 => {
            assert!(running_after(&watched, 1), "the sidecar restarted");
            tool_call_sse("whoami")
        }
        _ => DONE.to_owned(),
    });
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.acme]\nbase_url = \"{url}\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n"
        ),
    );
    ok(&run(
        home.path(),
        &["config", "set", "budgets.allow_unpriced", "true"],
    ));
    let out = run(
        home.path(),
        &["-z", "go", "--provider", "acme", "--model", "r1"],
    );
    assert_eq!(stdout(&out), "done\n", "{}", stderr(&out));
    let bodies: Vec<String> = bodies.try_iter().collect();
    assert_eq!(bodies.len(), 3, "{bodies:?}");
    let first = cached_prefix(&bodies[0]);
    assert!(first.0.contains("\"name\":\"whoami\""), "{}", first.0);
    assert!(
        first.1.contains("\"role\":\"system\"") && first.1.contains("Echo plugin rules."),
        "{}",
        first.1
    );
    for body in &bodies[1..] {
        assert_eq!(cached_prefix(body), first, "the cached prefix changed");
    }
    // The restart did drop the tool: the session's call answers by name.
    assert!(
        bodies[2].contains("tool 'whoami' is no longer provided by plugin 'echo'"),
        "{}",
        bodies[2]
    );
}

/// Scenario: Memory provider runs in a sidecar (spec: python-bridge, Scope
/// of hosted code): a Hermes `MemoryProvider` named by `memory.provider`
/// runs in its plugin's sidecar, and its recall reaches the turn.
#[test]
fn a_hermes_memory_provider_runs_in_a_sidecar() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    install(home.path(), "mempy", "mempy", "all");
    ok(&run(
        home.path(),
        &["config", "set", "memory.provider", "mempy"],
    ));
    let out = chat(
        home.path(),
        r#"
[[rule]]
user_regex = 'py-recalled: go'
text = "recall from python"

[[rule]]
repeat = true
text = "no recall"
"#,
    );
    assert_eq!(stdout(&out), "recall from python\n", "{}", stderr(&out));
    let status = ok(&run(home.path(), &["memory", "status"]));
    assert!(status.contains("provider: mempy"), "{status}");
}
