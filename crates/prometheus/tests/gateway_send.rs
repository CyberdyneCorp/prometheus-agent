//! One-shot sends, home channels, the emergency pause, chat policies and
//! the `send_message` tool in the real daemon, driven through the
//! debug-build file adapter (spec: agent-interop, One-shot send command;
//! `send_message` tool; messaging-gateway, Home channel and /sethome; Global
//! emergency pause; Per-chat policies).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{
    Daemon, adapter_dir, gateway_home, gateway_home_with, send, sent, texts, wait, wait_said,
};
use self::common::{command, ok, run, run_cmd_with_stdin, stderr, stdout};

const PAUSED: &str =
    "Paused: billing incident. Run `prometheus pause off` (or /pause off) to resume.";

fn user_messages(home: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT count(*) FROM messages WHERE role = 'user'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn sent_to(home: &Path, chat: &str) -> Vec<Value> {
    sent(home, "default")
        .into_iter()
        .filter(|v| v["kind"] == "send" && v["chat"] == chat)
        .collect()
}

#[test]
fn piping_a_status_line_sends_it_through_the_daemon_without_a_turn() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    let out = run_cmd_with_stdin(
        command(home).args(["gateway", "send", "--to", "test:-1001234567890", "-q"]),
        "RAM 92%\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "", "-q prints nothing");
    let messages = sent_to(home, "-1001234567890");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["text"], "RAM 92%");
    assert_eq!(user_messages(home), 0, "no agent turn");
    daemon.stop(home);
}

#[test]
fn without_the_daemon_a_stateless_adapter_sends_directly() {
    let home = gateway_home("");
    let home = home.path();
    let out = run_cmd_with_stdin(
        command(home)
            .args(["gateway", "send", "--to", "test:-1001234567890", "-q"])
            .env("PMT_TEST_ADAPTER_DIR", adapter_dir(home)),
        "RAM 92%\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(sent_to(home, "-1001234567890")[0]["text"], "RAM 92%");
}

#[test]
fn send_exit_codes_name_the_problem() {
    let home = gateway_home("\n[gateway.platforms.whatsapp]\nenabled = true\nallow_all = true\n");
    let home = home.path();
    let cases: [(&[&str], i32, &str); 4] = [
        (
            &["gateway", "send", "--to", "test", "hi"],
            66,
            "no home channel for test; run /sethome in the target chat",
        ),
        (
            &["gateway", "send", "--to", "signal:1", "hi"],
            78,
            "platform 'signal' is not configured in this profile",
        ),
        (
            // The bridge runs only inside the daemon: no stateless send.
            &["gateway", "send", "--to", "whatsapp:1", "hi"],
            69,
            "daemon required for whatsapp",
        ),
        (
            &["gateway", "send", "--to", "test:1"],
            2,
            "the message is empty",
        ),
    ];
    for (args, code, message) in cases {
        let out = command(home)
            .args(args)
            .env("PMT_TEST_ADAPTER_DIR", adapter_dir(home))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(code), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(message), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn sethome_makes_a_platform_only_send_go_to_that_chat() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    let err = run(home, &["gateway", "send", "--to", "test", "nightly report"]);
    assert_eq!(err.status.code(), Some(66), "{}", stderr(&err));
    send(home, "default", "ops", "/sethome");
    wait_said(home, "default", "Home channel set to ops");
    wait("the config reload", Duration::from_secs(30), || {
        run(home, &["gateway", "send", "--to", "test", "nightly report"])
            .status
            .success()
    });
    assert_eq!(
        sent_to(home, "ops").last().unwrap()["text"],
        "nightly report"
    );
    let list = ok(&run(home, &["gateway", "send", "--list"]));
    assert!(list.contains("test:ops  home"), "{list}");
    daemon.stop(home);
}

#[test]
fn a_pause_refuses_new_turns_across_a_restart_until_lifted() {
    let home = gateway_home("");
    let home = home.path();
    let paused = ok(&run(home, &["pause", "--reason", "billing incident"]));
    assert!(paused.contains("paused: billing incident"), "{paused}");
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "summarize my inbox");
    wait_said(home, "default", PAUSED);
    assert_eq!(user_messages(home), 0);
    daemon.stop(home);
    let oneshot = run(home, &["chat", "-z", "hello"]);
    assert_eq!(oneshot.status.code(), Some(69), "{}", stderr(&oneshot));
    assert!(stderr(&oneshot).contains(PAUSED), "{}", stderr(&oneshot));
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "still there?");
    wait("a second refusal", Duration::from_secs(30), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.as_str() == PAUSED)
            .count()
            == 2
    });
    let status = ok(&run(home, &["pause", "status"]));
    assert!(status.contains("by cli: billing incident"), "{status}");
    assert_eq!(ok(&run(home, &["pause", "off"])), "pause lifted\n");
    send(home, "default", "u1", "and now?");
    wait_said(home, "default", "fast done");
    daemon.stop(home);
}

#[test]
fn a_group_policy_gives_its_sessions_only_the_policy_toolsets() {
    let home = gateway_home(
        "\n[[gateway.chat_policies]]\nplatform = \"test\"\nchat_id = \"555\"\ntoolsets = [\"web\"]\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    let inbox = adapter_dir(home).join("default/inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = serde_json::json!({"id": "g1", "sender": "u1", "chat": "555",
        "chat_type": "group", "text": "hello group", "mentioned": true});
    std::fs::write(inbox.join("000900.json"), message.to_string()).unwrap();
    wait_said(home, "default", "fast done");
    // The surface the session froze: every tool with its exposure.
    let surface: String = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("SELECT tools FROM session_tool_surface", [], |r| r.get(0))
    .unwrap();
    let surface: Vec<Value> = serde_json::from_str(&surface).unwrap();
    let exposure = |name: &str| {
        surface
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| t["exposure"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default()
    };
    assert_ne!(exposure("web_search"), "not_in_toolset", "{surface:?}");
    for outside in ["terminal", "read_file", "write_file"] {
        assert_eq!(
            exposure(outside),
            "not_in_toolset",
            "{outside}: {surface:?}"
        );
    }
    daemon.stop(home);
}

const TOOL_RULES: &str = r#"
[[rule]]
user_regex = '^bad$'
[[rule.tool_calls]]
name = "send_message"
arguments = { target = "test:999", message = "leak" }

[[rule]]
tool_result_regex = 'test:999 is not an authorized destination'
text = "tool refused"

[[rule]]
user_regex = '^good$'
[[rule.tool_calls]]
name = "send_message"
arguments = { target = "test:u1", message = "a note for you" }

[[rule]]
tool_result_regex = 'Sent to test:u1'
text = "tool sent"

[[rule]]
repeat = true
text = "fast done"
"#;

#[test]
fn the_send_tool_refuses_unknown_chats_and_reaches_known_ones() {
    let home = gateway_home_with(
        TOOL_RULES,
        "\n[tools]\nenabled_toolsets = [\"core\", \"messaging\"]\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "bad");
    wait_said(home, "default", "tool refused");
    assert!(
        sent_to(home, "999").is_empty(),
        "nothing reached the unknown chat"
    );
    send(home, "default", "u1", "good");
    wait_said(home, "default", "tool sent");
    assert!(
        sent_to(home, "u1")
            .iter()
            .any(|m| m["text"] == "a note for you"),
        "{:?}",
        sent(home, "default")
    );
    daemon.stop(home);
}

/// Sending a file is reading it out: the file tools' dotenv refusal holds
/// for `send_message` attachments too.
#[test]
fn the_send_tool_refuses_to_attach_a_dotenv_file() {
    let project = tempfile::Builder::new()
        .prefix("pmt-send-env")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let dotenv = project.path().join(".env");
    std::fs::write(&dotenv, "API_KEY=sk-live").unwrap();
    let rules = format!(
        r#"
[[rule]]
user_regex = '^leak$'
[[rule.tool_calls]]
name = "send_message"
arguments = {{ target = "test:u1", message = "keys\nMEDIA:{}" }}

[[rule]]
tool_result_regex = 'is not a file this session may read'
text = "env refused"

[[rule]]
repeat = true
text = "fast done"
"#,
        dotenv.display().to_string().replace('\\', "/")
    );
    let home = gateway_home_with(
        &rules,
        "\n[tools]\nenabled_toolsets = [\"core\", \"messaging\"]\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "leak");
    wait_said(home, "default", "env refused");
    assert!(
        sent_to(home, "u1").iter().all(|m| m["text"] != "keys"),
        "{:?}",
        sent(home, "default")
    );
    daemon.stop(home);
}

#[test]
fn a_policy_naming_an_unknown_skill_fails_validation_by_name() {
    let home = gateway_home(
        "\n[[gateway.chat_policies]]\nplatform = \"test\"\nchat_id = \"555\"\nskills = [\"prometheus-agent\", \"no-such-skill\"]\n",
    );
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("gateway.chat_policies[0].skills: unknown skill 'no-such-skill'"),
        "{}",
        stderr(&out)
    );
    let skill = home.path().join("profiles/default/skills/triage/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: triage\ndescription: Sort incoming reports.\n---\nSort them.\n",
    )
    .unwrap();
    let config = home.path().join("profiles/default/config.toml");
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("no-such-skill", "triage");
    std::fs::write(&config, text).unwrap();
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}
