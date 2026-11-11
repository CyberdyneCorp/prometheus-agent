//! An external memory provider and a plugin context engine in real
//! sessions, over the test WASM plugins `testing-memory` and
//! `testing-engine` (spec: memory, External memory provider contract; One
//! external provider, isolated and bounded; Provider tools and prompt block
//! gating; Provider egress redaction and write mirroring; Profile-bound
//! providers; Pre-compress checkpoint; Unavailable or disabled provider
//! degrades loudly once; Provider setup writes secrets and settings to the
//! right stores; Context engine selection).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{append_config, mock_home, newest_session, ok, run, run_with_stdin, stderr, stdout};

const PROVIDER: &str = "testing-memory";

fn plugin_dir(home: &Path, profile: &str, id: &str) -> PathBuf {
    home.join("profiles").join(profile).join("plugins").join(id)
}

/// Places plugin `id` (crate `component`, `world`) in `profile` and enables
/// it with every grant.
fn install(home: &Path, profile: &str, id: &str, component: &str, world: &str) {
    let dir = plugin_dir(home, profile, id);
    pmt_plugins::testkit::install(component, &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"{world}\"]\n\n[capabilities]\n"
        ),
    )
    .unwrap();
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-08T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
    ok(&run(
        home,
        &["-p", profile, "plugins", "enable", id, "--grant", "all"],
    ));
}

/// A mock home whose default profile runs the testing provider.
fn provider_home(extra: &str) -> tempfile::TempDir {
    let home = mock_home();
    install(
        home.path(),
        "default",
        PROVIDER,
        "testing-memory",
        "memory-provider@1.0",
    );
    append_config(
        home.path(),
        &format!("\n[memory]\nprovider = \"{PROVIDER}\"\n{extra}"),
    );
    home
}

/// The provider's state value `key` in `profile`.
fn state(home: &Path, profile: &str, key: &str) -> String {
    pmt_plugins::host::state::StateDir::new(&home.join("profiles").join(profile), PROVIDER)
        .get(key)
        .unwrap()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// How often `needle` appears in the logs of the home and of every profile.
fn logs_mentioning(home: &Path, needle: &str) -> usize {
    let profiles = std::fs::read_dir(home.join("profiles"))
        .into_iter()
        .flat_map(|entries| entries.flatten().map(|e| e.path().join("logs")));
    std::iter::once(home.join("logs"))
        .chain(profiles)
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten().map(|e| e.path()))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.matches(needle).count())
        .sum()
}

/// Scenario: Unknown provider is a config error.
#[test]
fn an_unknown_provider_fails_validation_naming_the_key() {
    let home = mock_home();
    append_config(home.path(), "\n[memory]\nprovider = \"mem0\"\n");
    for args in [&["config", "validate"][..], &["-z", "hello"][..]] {
        let out = run(home.path(), args);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(78), "{err}");
        assert!(err.contains("memory.provider"), "{err}");
        assert!(err.contains("prometheus plugins install mem0"), "{err}");
    }
}

/// Scenario: Slow provider does not stall the turn.
#[test]
fn a_slow_prefetch_is_cut_at_its_bound_and_the_turn_goes_on() {
    let home = provider_home("provider_prefetch_timeout_ms = 1000\n");
    let started = Instant::now();
    let out = run(home.path(), &["-z", "a slow question"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("mock: a slow question"));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the turn waited {:?}",
        started.elapsed()
    );
    let files: Vec<String> = [
        home.path().join("logs"),
        home.path().join("profiles/default/logs"),
    ]
    .iter()
    .filter_map(|dir| std::fs::read_dir(dir).ok())
    .flat_map(|e| e.flatten().map(|e| e.path().display().to_string()))
    .collect();
    assert!(
        logs_mentioning(home.path(), "prefetch timed out") >= 1,
        "a WARN records the timeout: {} {files:?}",
        stderr(&out)
    );
}

/// A Chat Completions server answering `pong`, sending back each request
/// body.
fn server() -> (String, std::sync::mpsc::Receiver<serde_json::Value>) {
    use std::io::{BufReader, Write as _};
    const ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let body = common::read_http_request(&mut BufReader::new(stream.try_clone().unwrap()));
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or_default());
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ANSWER}",
                ANSWER.len()
            );
        }
    });
    (format!("http://{addr}/v1"), rx)
}

/// Points the default profile at `url` (a Chat Completions server).
fn served(home: &Path, url: &str) {
    append_config(
        home,
        &format!(
            "\n[providers.acme]\nbase_url = \"{url}\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\n\
             [budgets]\nallow_unpriced = true\n"
        ),
    );
    ok(&run(home, &["config", "set", "model.provider", "acme"]));
    ok(&run(home, &["config", "set", "model.default", "r1"]));
}

/// The provider's recall reaches the request inside the fence, its prompt
/// block the system prompt.
#[test]
fn provider_recall_and_prompt_block_reach_the_model() {
    let home = provider_home("");
    let (url, bodies) = server();
    served(home.path(), &url);
    ok(&run(home.path(), &["-z", "what about deploys"]));
    let sent = bodies.try_iter().next().expect("one request").to_string();
    assert!(sent.contains("TESTING MEMORY BLOCK"), "{sent}");
    assert!(sent.contains("<memory-context>"), "{sent}");
    assert!(sent.contains("remembered: what about deploys"), "{sent}");
}

/// Scenario: Provider cannot shadow a core tool.
#[test]
fn a_provider_tool_named_like_a_core_tool_is_refused_and_reported() {
    let home = provider_home("");
    let status = ok(&run(home.path(), &["memory", "status"]));
    assert!(status.contains("provider: testing-memory"), "{status}");
    assert!(
        status.contains(
            "tool 'memory' from plugin 'testing-memory' refused: already registered by built-in"
        ),
        "{status}"
    );
    let tools = ok(&run(home.path(), &["--json", "tools", "list"]));
    assert!(tools.contains("recall_notes"), "the other tool registers");
}

/// Scenario: API key in a turn is redacted for the provider.
#[test]
fn a_synced_turn_reaches_the_provider_redacted_and_the_transcript_keeps_it() {
    let home = provider_home("");
    ok(&run(
        home.path(),
        &["-z", "my key is sk-live-abcdef1234567890 keep it"],
    ));
    let synced = state(home.path(), "default", "synced");
    assert!(synced.contains("my key is"), "{synced}");
    assert!(!synced.contains("sk-live-abcdef1234567890"), "{synced}");
    assert!(synced.contains("[REDACTED"), "{synced}");
    // The egress pass never rewrites stored rows: the user row is what
    // the store recorded (its own known-secret redaction applied).
    let session = newest_session(home.path());
    let stored = common::session_messages(home.path(), &session);
    assert_eq!(
        stored[0]["content"], "my key is [REDACTED] keep it",
        "{stored:?}"
    );
    assert_eq!(state(home.path(), "default", "ended").trim(), "end");
}

const REMEMBER: &str = r#"
[[rule]]
user = "remember"
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Lives in Porto." }] }

[[rule]]
repeat = true
text = "noted"
"#;

/// Scenario: Multi-profile gateway keeps providers apart (one process
/// serves both profiles: the daemon).
#[cfg(feature = "gateway")]
#[test]
fn only_the_writing_profiles_provider_hears_the_write() {
    use common::gateway::{Daemon, gateway_home_with, send, wait_said};
    let home = gateway_home_with(REMEMBER, "");
    let home = home.path();
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    ok(&run(home, &["profile", "create", "b"]));
    let b = home.join("profiles/b/config.toml");
    let mut config = std::fs::read_to_string(&b).unwrap_or_default();
    config.push_str(
        "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[providers.mock]\nscript = \"mock.toml\"\n\n\
         [gateway.busy]\ndebounce_ms = 0\n\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    std::fs::write(&b, config).unwrap();
    std::fs::write(home.join("profiles/b/mock.toml"), REMEMBER).unwrap();
    for profile in ["default", "b"] {
        install(
            home,
            profile,
            PROVIDER,
            "testing-memory",
            "memory-provider@1.0",
        );
        ok(&run(
            home,
            &["-p", profile, "config", "set", "memory.provider", PROVIDER],
        ));
    }
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "noted");
    send(home, "b", "u2", "remember");
    wait_said(home, "b", "noted");
    daemon.stop(home);
    let written = state(home, "b", "writes");
    assert!(
        written.contains("add user") && written.contains("Lives in Porto."),
        "{written}"
    );
    assert_eq!(
        state(home, "default", "writes"),
        "",
        "profile a's provider heard nothing"
    );
}

/// Scenario: Required checkpoint failure keeps history.
#[cfg(unix)]
#[test]
fn a_failed_required_checkpoint_keeps_the_transcript_and_warns() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect as _;
    let home = provider_home("require_checkpoint = true\n");
    let script = home.path().join("long.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nrepeat = true\ntext = \"{}\"\n",
            "long answer ".repeat(200)
        ),
    )
    .unwrap();
    let script = script.to_string_lossy().into_owned();
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    for message in ["one", "two", "three", "four"] {
        prompt(&mut p);
        p.send_line(message).unwrap();
        p.expect("long answer").unwrap();
    }
    prompt(&mut p);
    // Keep only the last exchange: the rest is about to be summarized.
    p.send_line("/compress here 1").unwrap();
    p.expect("checkpoint_failed").unwrap();
    prompt(&mut p);
    exit(p);
    let session = newest_session(home.path());
    let messages = common::session_messages(home.path(), &session);
    assert_eq!(messages.len(), 8, "nothing was compacted: {messages:?}");
}

/// Scenario: Disabled provider warns once.
#[cfg(unix)]
#[test]
fn a_disabled_provider_warns_once_and_sessions_use_built_in_memory() {
    use common::pty::{exit, spawn};
    use expectrl::Expect as _;
    let home = provider_home("");
    ok(&run(home.path(), &["plugins", "disable", PROVIDER]));
    ok(&run(home.path(), &["-z", "hello"]));
    assert_eq!(
        logs_mentioning(home.path(), "prometheus plugins enable testing-memory"),
        1,
        "a one-shot logs the warning"
    );
    let mut p = spawn(home.path(), &[]);
    let mut seen = String::new();
    let mut step = |p: &mut expectrl::session::OsSession, until: &str| match p.expect(until) {
        Ok(found) => {
            seen.push_str(&String::from_utf8_lossy(found.before()));
            seen.push_str(until);
        }
        Err(err) => panic!("{err:?} waiting for {until:?} after {seen}"),
    };
    step(&mut p, "› ");
    p.send_line("hello there").unwrap();
    step(&mut p, "mock: hello there");
    step(&mut p, "› ");
    // `--yes` skips the destructive-command confirmation: its prompt can
    // render twice, and a `› ` match on the first render raced the answer.
    p.send_line("/new --yes").unwrap();
    step(&mut p, "new session");
    step(&mut p, "› ");
    p.send_line("again").unwrap();
    step(&mut p, "mock: again");
    step(&mut p, "› ");
    exit(p);
    assert_eq!(
        seen.matches("is disabled").count(),
        1,
        "warned once: {seen}"
    );
    assert_eq!(
        state(home.path(), "default", "sessions"),
        "",
        "never started"
    );
}

/// Scenario: Non-secret field never lands in secrets.
#[test]
fn setup_writes_the_secret_to_the_store_and_the_setting_to_config() {
    let home = mock_home();
    install(
        home.path(),
        "default",
        PROVIDER,
        "testing-memory",
        "memory-provider@1.0",
    );
    let out = run_with_stdin(
        home.path(),
        &["memory", "setup", PROVIDER],
        "key-0123456789\nacme\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("workspace = \"acme\""), "{config}");
    assert!(config.contains("provider = \"testing-memory\""), "{config}");
    assert!(!config.contains("key-0123456789"), "{config}");
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(secrets.contains("TESTING_MEMORY_API_KEY"), "{secrets}");
    assert!(!secrets.contains("WORKSPACE"), "{secrets}");
    ok(&run(home.path(), &["memory", "off"]));
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("provider = \"builtin\""), "{config}");
}

/// Scenario: Engine selection never rewrites stored history.
#[test]
fn a_plugin_engines_selection_shapes_only_the_request() {
    let home = mock_home();
    install(
        home.path(),
        "default",
        "engine",
        "testing-engine",
        "context-engine@1.0",
    );
    append_config(home.path(), "\n[context]\nengine = \"engine\"\n");
    let (url, bodies) = server();
    served(home.path(), &url);
    ok(&run(home.path(), &["-z", "first"]));
    let session = newest_session(home.path());
    ok(&run(home.path(), &["-z", "second", "--resume", &session]));
    ok(&run(home.path(), &["-z", "third", "--resume", &session]));
    // The conversation turns only (the system prompt lists skills whose
    // descriptions may hold any word).
    let sent: Vec<String> = bodies
        .try_iter()
        .map(|b| {
            let turns: Vec<&serde_json::Value> = b["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] != "system")
                .collect();
            serde_json::to_string(&turns).unwrap()
        })
        .collect();
    assert_eq!(sent.len(), 3);
    assert!(sent[2].contains("third"), "{}", sent[2]);
    assert!(
        !sent[2].contains("first"),
        "the engine dropped the oldest turn: {}",
        sent[2]
    );
    let stored: Vec<String> = common::session_messages(home.path(), &session)
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    for text in ["first", "second", "third"] {
        assert!(stored.iter().any(|t| t == text), "{text} kept: {stored:?}");
    }
}

/// An engine whose `initialize` fails leaves the session on the built-in
/// engine (no per-request selection) with one WARN.
#[test]
fn an_engine_that_fails_to_start_leaves_the_session_on_the_built_in_engine() {
    const PROFILE: &str = "engine-fails";
    let home = mock_home();
    let home = home.path();
    ok(&run(home, &["profile", "create", PROFILE]));
    install(
        home,
        PROFILE,
        "engine",
        "testing-engine",
        "context-engine@1.0",
    );
    let (url, bodies) = server();
    let config = home.join("profiles").join(PROFILE).join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let text = text
        + &format!(
            "\n[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n\
         [providers.acme]\nbase_url = \"{url}\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\n\
         [budgets]\nallow_unpriced = true\n"
        );
    std::fs::write(&config, text).unwrap();
    let chat = |args: &[&str]| ok(&run(home, &[&["-p", PROFILE][..], args].concat()));
    chat(&["-z", "first"]);
    let listed: serde_json::Value =
        serde_json::from_str(&chat(&["--json", "sessions", "list", "--all"])).unwrap();
    let session = listed["sessions"][0]["id"].as_str().unwrap().to_owned();
    chat(&["-z", "second", "--resume", &session]);
    chat(&["config", "set", "context.engine", "engine"]);
    chat(&["-z", "third", "--resume", &session]);
    // The conversation turns only (the system prompt lists skills whose
    // descriptions may hold any word).
    let sent: Vec<String> = bodies
        .try_iter()
        .map(|b| {
            let turns: Vec<&serde_json::Value> = b["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] != "system")
                .collect();
            serde_json::to_string(&turns).unwrap()
        })
        .collect();
    assert_eq!(sent.len(), 3);
    assert!(
        sent[2].contains("first") && sent[2].contains("third"),
        "the built-in engine sends the whole history: {}",
        sent[2]
    );
    assert_eq!(
        logs_mentioning(home, "context engine failed to start"),
        1,
        "one WARN"
    );
}

/// Scenario: Spill to file (External provider recall budget and spill).
#[test]
fn oversized_provider_recall_spills_to_a_file_under_cache() {
    let home = provider_home("provider_recall_spill = true\n");
    let (url, bodies) = server();
    served(home.path(), &url);
    ok(&run(home.path(), &["-z", "a big question"]));
    let sent = bodies.try_iter().next().expect("one request").to_string();
    assert!(
        sent.len() < 100_000,
        "the fence holds a preview, not 250000 characters"
    );
    let marker = "the full text is in ";
    let at = sent.find(marker).expect("the spill path") + marker.len();
    let path: String = sent[at..].chars().take_while(|c| *c != ' ').collect();
    assert!(path.contains("cache"), "{path}");
    let full = std::fs::read_to_string(&path).unwrap();
    assert_eq!(full.len(), 250_000);
}

/// Without spill, recall past `memory.provider_recall_max_chars` is cut
/// with a marker.
#[test]
fn oversized_provider_recall_is_cut_at_its_bound() {
    let home = provider_home("provider_recall_max_chars = 1000\n");
    let (url, bodies) = server();
    served(home.path(), &url);
    ok(&run(home.path(), &["-z", "a big question"]));
    let sent = bodies.try_iter().next().expect("one request").to_string();
    assert!(
        sent.contains("provider recall truncated: 249000 more characters"),
        "{}",
        &sent[..2000.min(sent.len())]
    );
}
