//! Task 66.1: the launch entry point, stdout carrying only frames, quiet
//! probes, configuration from flags and the profile, and the process as a
//! daemon viewer bound to its launch profile (spec: acp-adapter).

use std::time::Duration;

use serde_json::json;

use super::common::{self, daemon};
use super::fixture::{Acp, Home};

/// Scenario: Check without a model.
#[test]
fn check_names_the_missing_model_and_passes_once_one_is_set() {
    let home = common::temp_home();
    let out = common::run(home.path(), &["acp", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        common::stdout(&out).trim(),
        "no default model configured for profile default; run 'prometheus setup'"
    );
    let ready = Home::new();
    let out = common::run(ready.path(), &["acp", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::stdout(&out));
    assert_eq!(common::stdout(&out).trim(), "Prometheus ACP check OK");
}

#[test]
fn version_is_answered_before_any_server_starts() {
    let home = common::temp_home();
    let out = common::ok(&common::run(home.path(), &["acp", "--version"]));
    let plain = common::ok(&common::run(home.path(), &["--version"]));
    assert_eq!(out, plain);
}

/// Scenario: A component logs during a prompt. The secret a log record
/// carries is redacted on stderr and in `acp.log`; stdout holds frames
/// only, through a whole prompt at debug level.
#[test]
fn logs_go_to_stderr_and_acp_log_redacted_while_stdout_carries_frames() {
    let home = Home::new();
    home.config("\n[logging]\nlevel = \"debug\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let secret = "sk-acp-redact-me-0123456789";
    let mut acp = Acp::spawn(
        home.path(),
        &[],
        &[],
        &[("PMT_SECRET_ACP_TEST_TOKEN", secret)],
    );
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let id = acp.send_prompt(&session, "hello from the editor");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let unknown = acp.call(secret, &json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    acp.pump(Duration::from_millis(300));
    acp.assert_only_frames();
    let stderr = acp.stderr();
    assert!(stderr.contains("unknown ACP method"), "{stderr}");
    assert!(!stderr.contains(secret), "secret on stderr: {stderr}");
    let log = std::fs::read_to_string(home.path().join("profiles/default/logs/acp.log")).unwrap();
    assert!(log.contains("unknown ACP method"), "{log}");
    assert!(!log.contains(secret), "secret in acp.log");
}

/// Scenario: Editor probe.
#[test]
fn liveness_probes_answer_method_not_found_without_a_warning() {
    let home = Home::new();
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    for probe in ["ping", "health", "healthcheck"] {
        let answer = acp.call(probe, &json!({}));
        assert_eq!(answer["error"]["code"], -32601, "{answer}");
    }
    acp.pump(Duration::from_millis(300));
    let stderr = acp.stderr();
    assert!(
        !stderr.contains("warn"),
        "a probe was logged as a warning: {stderr}"
    );
    acp.call("frobnicate", &json!({}));
    acp.pump(Duration::from_millis(300));
    assert!(
        acp.stderr().contains("warn: unknown ACP method"),
        "{}",
        acp.stderr()
    );
}

/// Scenario: Safe mode without a model.
#[test]
fn safe_mode_without_model_and_provider_refuses_new_sessions() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &["--safe-mode"], &[], &[]);
    acp.initialize(&json!({}));
    let answer = acp.call("session/new", &json!({"cwd": home.work, "mcpServers": []}));
    assert_eq!(
        answer["error"]["message"],
        "--safe-mode reads no profile default model: launch with --model <m> --provider <p>"
    );
    let mut flagged = Acp::spawn(
        home.path(),
        &["--safe-mode"],
        &["--model", "mock", "--provider", "mock"],
        &[],
    );
    flagged.initialize(&json!({}));
    flagged.new_session(&home.work);
}

/// A non-secret environment variable changes nothing: `PMT_MODEL` names
/// a model, but the session runs the profile's.
#[test]
fn a_non_secret_environment_variable_does_not_change_the_session() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[("PMT_MODEL", "other-model")]);
    acp.initialize(&json!({}));
    let answer = acp.ok("session/new", &json!({"cwd": home.work, "mcpServers": []}));
    assert_eq!(answer["models"]["currentModelId"], "mock/mock");
}

/// Scenario: Editor closes mid-turn. The editor is killed while its
/// turn runs; the turn finishes in the daemon and a new editor loading
/// the session sees the answer.
#[test]
fn a_turn_survives_the_editor_and_shows_on_load() {
    let home = Home::new();
    home.script(
        "latency_ms = 400\n[[rule]]\ntext = \"slow answer from the daemon\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    acp.send_prompt(&session, "take your time");
    acp.wait_for("the turn's first chunk", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    acp.kill();
    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    loop {
        let transcript = home.rpc("session.transcript", &json!({"session_id": session}));
        let answered = transcript["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "assistant");
        if answered {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn never finished"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut again = Acp::spawn(home.path(), &[], &[], &[]);
    again.initialize(&json!({}));
    again.ok(
        "session/load",
        &json!({"sessionId": session, "cwd": home.work, "mcpServers": []}),
    );
    assert!(
        again
            .agent_text(&session)
            .contains("slow answer from the daemon"),
        "{:?}",
        again.updates(&session)
    );
}

/// Scenario: Wrong daemon. The running daemon serves only `default`;
/// an editor launched for profile `work` is refused with
/// `profile_mismatch`.
#[test]
fn a_daemon_without_the_launch_profile_is_refused() {
    let home = Home::new();
    common::ok(&common::run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        common::ok(&common::run(
            home.path(),
            &["-p", "work", "config", "set", key, value],
        ));
    }
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &["-p", "work"], &[], &[]);
    acp.initialize(&json!({}));
    let answer = acp.call("session/new", &json!({"cwd": home.work, "mcpServers": []}));
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("profile_mismatch"), "{answer}");
}

/// The daemon is started when none runs (`daemon.autostart`); closing
/// the editor leaves it running.
#[test]
fn the_daemon_is_started_for_the_first_session_and_outlives_the_editor() {
    let home = Home::new();
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    acp.new_session(&home.work);
    let record = daemon::record(home.path());
    let _daemon = daemon::Daemon {
        home: home.path(),
        pid: record.process.pid,
    };
    assert!(acp.close().success());
    assert!(matches!(
        pmt_rpc::discovery::discover(home.path()).unwrap(),
        pmt_rpc::discovery::Discovery::Running(_)
    ));
}
