//! What a `prometheus chat` session is built with, end to end against the
//! built binary on a temp home (spec: execution-environments, Typed
//! backend registry and selection; memory, Context engine selection;
//! model-providers, Model selection precedence, In-turn fallback
//! activation, Borrowed CLI logins; replay-and-evaluation, Deterministic
//! mock provider, Mock fault injection; skills, Skill secrets and
//! settings, Load-time template substitution and inline shell;
//! session-storage, Session records; cli, Machine-readable output).

mod common;

use std::fmt::Write as _;
use std::io::{BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{command, mock_home, ok, run, run_with_stdin, stderr, stdout, temp_home};
use serde_json::Value;

/// `chat <extra> -z <prompt>` from the home.
fn chat(home: &Path, extra: &[&str], prompt: &str) -> Output {
    command(home)
        .current_dir(home)
        .arg("chat")
        .args(extra)
        .args(["-z", prompt])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn script(home: &Path, text: &str) -> PathBuf {
    let path = home.join("s.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn append_config(home: &Path, text: &str) {
    let path = home.join("profiles/default/config.toml");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// The trajectory events of the only session of the home.
fn events(home: &Path) -> Vec<Value> {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    let id = listed["sessions"][0]["id"].as_str().unwrap().to_owned();
    ok(&run(
        home,
        &["trajectory", "show", &id, "--format", "jsonl"],
    ))
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

/// A selected backend that cannot be set up never falls back to the host
/// (spec: execution-environments, Typed backend registry and selection).
#[test]
fn a_container_backend_without_a_runtime_never_runs_on_the_host() {
    let home = mock_home();
    let marker = home.path().join("ran.txt");
    let s = script(
        home.path(),
        &format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = \"echo ran > '{}'\" }}\n[[rule]]\nstep = 1\ntext = \"done\"\n",
            marker.display()
        ),
    );
    ok(&run(
        home.path(),
        &["config", "set", "terminal.backend", "container"],
    ));
    let missing = home.path().join("no-such-runtime");
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "terminal.container.runtime",
            missing.to_str().unwrap(),
        ],
    ));
    let out = chat(home.path(), &["--mock-script", s.to_str().unwrap()], "go");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!marker.exists(), "the command ran on the host");
}

/// Scenario: Unknown backend.
#[test]
fn an_unknown_backend_exits_78_listing_the_available_ones() {
    let home = mock_home();
    append_config(home.path(), "\n[terminal]\nbackend = \"acmebox\"\n");
    let out = chat(home.path(), &[], "hi");
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out)
            .contains("unknown terminal backend 'acmebox'; available: local, container, ssh"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Unknown engine.
#[test]
fn an_unknown_context_engine_fails_the_chat_start_with_78() {
    let home = mock_home();
    let out = chat(home.path(), &["--set", "context.engine=lcm"], "hi");
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown context engine 'lcm'"),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty());
}

#[test]
fn the_model_flag_takes_a_provider_prefix() {
    let home = temp_home();
    let out = chat(home.path(), &[], "hi");
    assert_eq!(out.status.code(), Some(78), "no provider configured");
    let out = command(home.path())
        .current_dir(home.path())
        .args(["chat", "-m", "mock/mock", "-z", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ok(&out), "mock: hi\n");
    // The session records the route it ran on.
    let listed: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "list", "--all"],
    )))
    .unwrap();
    let session = &listed["sessions"][0];
    assert_eq!(
        (&session["provider"], &session["model"]),
        (&Value::from("mock"), &Value::from("mock"))
    );

    // Another provider named on the flag is used, not the configured one.
    let home = mock_home();
    let out = command(home.path())
        .current_dir(home.path())
        .args(["chat", "-m", "openai/gpt-4o", "-z", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("openai"), "{}", stderr(&out));
}

/// Scenario: Scripted tool call.
#[test]
fn a_scripted_tool_call_reads_the_file_once_and_prints_done() {
    let home = temp_home();
    std::fs::write(home.path().join("README.md"), "# Ledger\n").unwrap();
    let s = script(
        home.path(),
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = { path = \"README.md\" }\n\
         [[rule]]\nstep = 1\ntext = \"done\"\n",
    );
    let out = command(home.path())
        .current_dir(home.path())
        .args(["chat", "-z", "summarise", "--provider", "mock"])
        .args(["--mock-script", s.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ok(&out), "done\n");
    let calls: Vec<Value> = events(home.path())
        .into_iter()
        .filter(|e| e["type"] == "tool.call")
        .collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["payload"]["tool"], "read_file");
}

/// Scenario: Fallback on rate limit; the notice every surface shows.
#[test]
fn a_rate_limited_primary_falls_back_with_a_notice_and_a_trajectory_event() {
    let home = temp_home();
    ok(&run(home.path(), &["config", "path"]));
    append_config(
        home.path(),
        "[model]\nprovider = \"mock\"\ndefault = \"primary\"\n\n\
         [[fallback.chain]]\nprovider = \"mock\"\nmodel = \"secondary\"\n",
    );
    let s = script(
        home.path(),
        "[[rule]]\nmodel = \"primary\"\nfault = \"rate_limit\"\nrepeat = true\n\
         [[rule]]\nmodel = \"secondary\"\ntext = \"answered by the secondary\"\nrepeat = true\n",
    );
    let out = command(home.path())
        .current_dir(home.path())
        .args(["chat", "--mock-script", s.to_str().unwrap(), "-z", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ok(&out), "answered by the secondary\n");
    assert!(
        stderr(&out).contains("switched from mock/primary to mock/secondary (rate_limit)"),
        "{}",
        stderr(&out)
    );
    let fallback = events(home.path())
        .into_iter()
        .find(|e| e["type"] == "provider.fallback")
        .expect("a provider.fallback event");
    assert_eq!(fallback["payload"]["from"], "mock/primary");
    assert_eq!(fallback["payload"]["to"], "mock/secondary");
    assert_eq!(fallback["payload"]["reason"], "rate_limit");
}

#[test]
fn a_failed_json_run_writes_the_error_document_to_stderr() {
    let home = mock_home();
    let s = script(
        home.path(),
        "[[rule]]\nfault = \"auth_error\"\nrepeat = true\n",
    );
    let out = command(home.path())
        .current_dir(home.path())
        .args(["--json", "chat", "--mock-script", s.to_str().unwrap()])
        .args(["-z", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    let report: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_ne!(report["exit_reason"], "completed");
    let error: Value = serde_json::from_str(stderr(&out).trim())
        .unwrap_or_else(|e| panic!("{e}: {}", stderr(&out)));
    assert!(error["error"]["code"].is_string(), "{error}");
    assert!(error["error"]["message"].is_string(), "{error}");
}

const INLINE: &str =
    "---\nname: probe\ndescription: Runs a probe.\n---\nProbe says !`echo INLINE_OK`.\n";
const NOTION: &str = "---\nname: notion\ndescription: Notion pages.\nmetadata:\n  prometheus:\n    secrets:\n      - name: NOTION_API_KEY\n---\nUse the API.\n";

fn skill(home: &Path, name: &str, text: &str) {
    let dir = home.join("profiles/default/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), text).unwrap();
}

/// The skills of a real session run snippets in its sandbox and pass a
/// loaded skill's declared secret to its terminal.
#[test]
fn a_session_runs_skill_snippets_and_passes_declared_secrets() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "skills.inline_shell", "true"],
    ));
    skill(home.path(), "probe", INLINE);
    skill(home.path(), "notion", NOTION);
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "NOTION_API_KEY"],
        "ntn-test-value-123",
    ));
    let s = script(
        home.path(),
        r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "skill_view"
arguments = { name = "probe" }

[[rule]]
step = 1
tool_result_regex = "Probe says INLINE_OK"
[[rule.tool_calls]]
name = "skill_view"
arguments = { name = "notion" }

[[rule]]
step = 2
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "test -n \"$NOTION_API_KEY\" && echo KEY_SET || echo KEY_EMPTY" }

[[rule]]
step = 3
tool_result_regex = "KEY_SET"
text = "inline and secret ok"

[[rule]]
text = "missing"
"#,
    );
    let out = chat(
        home.path(),
        &["--yolo", "--mock-script", s.to_str().unwrap()],
        "go",
    );
    assert_eq!(ok(&out), "inline and secret ok\n", "{}", stderr(&out));
}

/// Serves one Anthropic Messages stream answering `hello`.
fn anthropic_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            common::read_http_request(&mut reader);
            let events = [
                r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ];
            let mut sse = String::new();
            for data in events {
                let kind: Value = serde_json::from_str(data).unwrap();
                let _ = write!(
                    sse,
                    "event: {}\ndata: {data}\n\n",
                    kind["type"].as_str().unwrap()
                );
            }
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
        }
    });
    format!("http://{addr}")
}

/// A borrowed Claude Code login announces its first use in the session.
#[test]
fn a_borrowed_login_is_announced_on_first_use() {
    let home = temp_home();
    let claude = common::user_home(home.path()).join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let login = serde_json::json!({"claudeAiOauth": {
        "accessToken": "sk-ant-oat01-borrowed", "refreshToken": "sk-ant-ort01-borrowed",
        "expiresAt": 4_102_444_800_000_i64, "subscriptionType": "max"
    }});
    std::fs::write(claude.join(".credentials.json"), login.to_string()).unwrap();
    ok(&run(home.path(), &["config", "path"]));
    append_config(
        home.path(),
        &format!(
            "[model]\nprovider = \"anthropic\"\ndefault = \"claude-sonnet-4-5\"\n\n\
             [providers.anthropic]\nbase_url = \"{}\"\nkey_hosts = [\"127.0.0.1\"]\n\
             wire_format = \"anthropic_messages\"\n",
            anthropic_server()
        ),
    );
    let out = chat(home.path(), &[], "hi");
    assert_eq!(ok(&out), "hello\n");
    let err = stderr(&out);
    assert_eq!(
        err.matches("borrowed:claude-code").count(),
        1,
        "announced once: {err}"
    );
    assert!(err.contains("Claude Code"), "{err}");
    assert!(!err.contains("sk-ant-oat01-borrowed"), "{err}");
}

/// Daily maintenance runs at session start (spec: session-storage,
/// Retention, pruning and auto-archive; observability, Trajectory
/// retention and size limits).
#[test]
fn a_session_start_runs_the_daily_maintenance() {
    let home = mock_home();
    ok(&chat(home.path(), &[], "first"));
    let old = common::newest_session(home.path());
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let long_ago = jiff::Timestamp::now().as_millisecond() - 100 * 86_400_000;
    db.execute(
        "UPDATE sessions SET last_activity_at = ?1, ended_at = ?1 WHERE id = ?2",
        rusqlite::params![long_ago, old],
    )
    .unwrap();
    db.execute(
        "DELETE FROM store_meta WHERE key = 'last_maintenance_at'",
        [],
    )
    .unwrap();
    drop(db);
    ok(&chat(home.path(), &[], "second"));
    let listed: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "list", "--all"],
    )))
    .unwrap();
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == old.as_str())
        .unwrap();
    assert_eq!(row["archived"], "auto", "{row}");
    let out = run(home.path(), &["trajectory", "show", &old]);
    assert!(
        !out.status.success(),
        "the 100-day-old trajectory was purged"
    );
}
