//! The messaging gateway inside the real daemon, driven through the
//! debug-build file adapter (`PMT_TEST_ADAPTER_DIR`, platform `test`)
//! (spec: messaging-gateway, Crash recovery and clean shutdown marker;
//! Conversations never rotate on time; Agent cache and eviction;
//! Documented pairing flow; Gateway slash commands, quick commands and
//! hooks; Profiles served by one daemon; Background and internal
//! notifications).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use self::common::gateway::{
    Daemon, gateway_home, gateway_home_with, send, texts, wait, wait_said,
};
use self::common::{ok, run, stderr};

const RESTARTED: &str =
    "The agent restarted while working on your last message. Reply /retry to run it again.";

fn db(home: &Path, profile: &str) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        home.join("profiles").join(profile).join("state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

/// How many user messages the default profile stored.
fn stored_prompts(home: &Path) -> i64 {
    db(home, "default")
        .query_row(
            "SELECT count(*) FROM messages WHERE role = 'user'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(session, turn_active, resume_pending, restart_count)` of the routed
/// sessions.
fn routed(home: &Path) -> Vec<(String, i64, i64, i64)> {
    let conn = db(home, "default");
    let mut stmt = conn
        .prepare(
            "SELECT session_id, turn_active, resume_pending, restart_count FROM gateway_sessions \
             ORDER BY rowid",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_daemon_killed_mid_turn_recovers_the_conversation() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "slow");
    // Killed once the prompt is stored: repair then merges it into the next
    // message's request, as after a real crash mid-turn.
    wait("the turn runs", Duration::from_secs(30), || {
        routed(home).first().is_some_and(|r| r.1 == 1) && stored_prompts(home) == 1
    });
    daemon.kill();
    assert!(!home.join("run/clean_shutdown").exists());
    let daemon = Daemon::start(home);
    wait_said(home, "default", RESTARTED);
    let state = routed(home);
    assert_eq!(state.len(), 1);
    assert_eq!((state[0].1, state[0].2, state[0].3), (0, 1, 1), "{state:?}");
    // The interrupted turn holds the conversation until it is dropped.
    send(home, "default", "u1", "/discard");
    wait_said(home, "default", "Dropped the interrupted turn.");
    send(home, "default", "u1", "are you there?");
    wait_said(home, "default", "fast done");
    let after = routed(home);
    assert_eq!(after.len(), 1, "the conversation keeps its session");
    assert_eq!(after[0].0, state[0].0);
    assert_eq!(after[0].2, 0, "a completed turn clears the pending resume");
    let notices = texts(home, "default")
        .iter()
        .filter(|t| t.as_str() == RESTARTED)
        .count();
    assert_eq!(notices, 1);
    assert_eq!(daemon.stop(home), Some(0));
    assert!(
        home.join("run/clean_shutdown").exists(),
        "a drained stop leaves the marker"
    );
    let daemon = Daemon::start(home);
    assert!(
        !home.join("run/clean_shutdown").exists(),
        "the next start consumes it"
    );
    daemon.stop(home);
    assert_eq!(
        texts(home, "default")
            .iter()
            .filter(|t| t.as_str() == RESTARTED)
            .count(),
        1,
        "no recovery after a clean stop"
    );
}

#[test]
fn an_evicted_agent_is_rebuilt_with_the_same_frozen_prompt() {
    let home = gateway_home("\n[gateway.agent_cache]\nidle_ttl_secs = 2\n");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    let frozen = || -> (String, String, i64) {
        db(home, "default")
            .query_row(
                "SELECT sha256, tools, built_at FROM session_prompts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    let before = frozen();
    wait(
        "the idle agent is unloaded",
        Duration::from_secs(30),
        || {
            let out = run(home, &["--json", "daemon", "status"]);
            let status: Value = serde_json::from_str(&common::stdout(&out)).unwrap_or_default();
            status["daemon"]["sessions"] == 0
        },
    );
    send(home, "default", "u1", "hello again");
    wait("a second answer", Duration::from_mins(1), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.contains("fast done"))
            .count()
            == 2
    });
    assert_eq!(frozen(), before, "the prompt and tools were not rebuilt");
    let sessions: i64 = db(home, "default")
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 1, "eviction never ends or replaces the session");
    daemon.stop(home);
}

// The quick command is a POSIX shell pipeline.
#[cfg(unix)]
#[test]
fn a_gateway_quick_command_is_killed_after_its_timeout() {
    let home = gateway_home(
        "\n[gateway.quick_commands]\ntimeout_secs = 2\n\n[quick_commands.slow]\ntype = \"exec\"\n\
         command = \"sleep 120 | sleep 120\"\ntimeout_secs = 600\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    let started = Instant::now();
    send(home, "default", "u1", "/slow");
    wait_said(home, "default", "quick command 'slow' timed out after 2s");
    assert!(started.elapsed() < Duration::from_secs(30));
    daemon.stop(home);
}

/// The 8-character code in the pairing reply.
fn code_of(text: &str) -> String {
    text.split("Your pairing code is ")
        .nth(1)
        .unwrap()
        .chars()
        .take(8)
        .collect()
}

#[test]
fn a_pairing_code_is_approved_from_the_cli_whatever_its_formatting() {
    let home = gateway_home("");
    let home = home.path();
    ok(&run(home, &["profile", "create", "work"]));
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    let work = home.join("profiles/work/config.toml");
    let mut config = std::fs::read_to_string(&work).unwrap_or_default();
    config.push_str("\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[gateway.platforms.test]\nenabled = true\n");
    std::fs::write(&work, config).unwrap();
    let daemon = Daemon::start(home);
    send(home, "work", "stranger", "hi");
    wait_said(home, "work", "Your pairing code is ");
    let code = code_of(&texts(home, "work")[0]);
    let mine = ok(&run(home, &["-p", "work", "pairing", "list"]));
    assert!(mine.contains("test  pending  stranger"), "{mine}");
    let default = ok(&run(home, &["pairing", "list"]));
    assert!(default.trim().is_empty(), "{default}");
    let typed = format!("{} {}", &code[..4], &code[4..]).to_lowercase();
    let out = run(home, &["-p", "work", "pairing", "approve", "test", &typed]);
    ok(&out);
    assert!(
        stderr(&out).contains("approved stranger on test"),
        "{}",
        stderr(&out)
    );
    wait_said(home, "work", "You are paired");
    send(home, "work", "stranger", "hello");
    wait_said(home, "work", "mock: hello");
    let again = run(home, &["-p", "work", "pairing", "approve", "test", &code]);
    assert_eq!(again.status.code(), Some(66), "{}", stderr(&again));
    daemon.stop(home);
}

#[test]
fn gateway_status_reports_each_adapter() {
    let home = gateway_home("");
    let home = home.path();
    let missing = run(home, &["gateway", "status"]);
    assert_eq!(missing.status.code(), Some(69), "{}", stderr(&missing));
    let daemon = Daemon::start(home);
    wait("the adapter is up", Duration::from_secs(30), || {
        ok(&run(home, &["gateway", "status"])).contains("test: degraded")
    });
    let text = ok(&run(home, &["gateway", "status"]));
    assert!(
        text.contains("connected but not yet confirmed active; recovering in background"),
        "{text}"
    );
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    let text = ok(&run(home, &["gateway", "status"]));
    assert!(text.contains("default:\n  test: connected"), "{text}");
    let outbox = ok(&run(home, &["gateway", "outbox", "list"]));
    assert!(outbox.contains("sent  reply  test:u1"), "{outbox}");
    daemon.stop(home);
}

#[test]
fn gateway_platforms_prints_each_loaded_descriptor() {
    let home = gateway_home("max_message_length = 1000\n");
    let home = home.path();
    let daemon = Daemon::start(home);
    wait("the adapter is up", Duration::from_secs(30), || {
        ok(&run(home, &["gateway", "platforms"])).contains("default/test:")
    });
    let text = ok(&run(home, &["gateway", "platforms"]));
    assert!(text.contains("  max_message_length = 1000\n"), "{text}");
    assert!(text.contains("  length_unit = \"chars\"\n"), "{text}");
    assert!(text.contains("  interactive = \"native\"\n"), "{text}");
    let json = ok(&run(home, &["--json", "gateway", "platforms"]));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        value["platforms"][0]["descriptor"]["max_message_length"],
        1000
    );
    daemon.stop(home);
}

#[test]
fn a_profile_created_while_running_is_served_after_gateway_reload() {
    let home = gateway_home("");
    let home = home.path();
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    let before = routed(home);
    ok(&run(home, &["profile", "create", "side"]));
    let side = home.join("profiles/side/config.toml");
    let mut config = std::fs::read_to_string(&side).unwrap_or_default();
    config.push_str(
        "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[gateway.busy]\ndebounce_ms = 0\n\n\
         [gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    std::fs::write(&side, config).unwrap();
    let status = ok(&run(home, &["gateway", "reload"]));
    assert!(status.contains("side:"), "{status}");
    assert!(
        status.contains("default:\n  test: connected"),
        "the running adapter was not restarted: {status}"
    );
    send(home, "side", "s1", "hi side");
    wait_said(home, "side", "mock: hi side");
    send(home, "default", "u1", "still here?");
    wait("a second default answer", Duration::from_mins(1), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.contains("fast done"))
            .count()
            == 2
    });
    assert_eq!(
        routed(home)[0].0,
        before[0].0,
        "the default session goes on"
    );
    daemon.stop(home);
}

/// A turn that starts background processes with `notify_on_complete`.
const BACKGROUND_RULES: &str = r#"
[[rule]]
user_regex = '^bg$'
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "sleep 1; exit 0", background = true, notify_on_complete = true }

[[rule]]
user_regex = '^bgfail$'
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "sleep 1; exit 3", background = true, notify_on_complete = true }

[[rule]]
tool_result_regex = 'process_id'
text = "started bg"

[[rule]]
repeat = true
text = "fast done"
"#;

/// Spec: messaging-gateway, Background and internal notifications. The
/// completion of a background process reaches the chat whose turn started
/// it, filtered by `gateway.notifications.background`.
#[test]
fn a_background_process_completion_reaches_the_chat() {
    let home = gateway_home_with(
        BACKGROUND_RULES,
        "\n[gateway.notifications]\nbackground = \"all\"\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "bg");
    wait_said(home, "default", "started bg");
    wait_said(home, "default", "Background process");
    let note = texts(home, "default")
        .into_iter()
        .find(|t| t.starts_with("Background process"))
        .unwrap();
    assert!(note.contains("finished: sleep 1; exit 0"), "{note}");
    daemon.stop(home);
}

/// Spec: messaging-gateway, Background and internal notifications, Errors
/// only: a process that exits 0 is not announced; one that fails is.
#[test]
fn errors_only_announces_the_failed_background_process_alone() {
    let home = gateway_home_with(
        BACKGROUND_RULES,
        "\n[gateway.notifications]\nbackground = \"error\"\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "bg");
    wait_said(home, "default", "started bg");
    send(home, "default", "u1", "bgfail");
    wait_said(home, "default", "failed (exit 3)");
    let notes: Vec<String> = texts(home, "default")
        .into_iter()
        .filter(|t| t.starts_with("Background process"))
        .collect();
    assert_eq!(notes.len(), 1, "only the failure: {notes:?}");
    daemon.stop(home);
}

/// The `compression.committed` events of the default profile's live
/// trajectories.
fn compactions(home: &Path) -> Vec<String> {
    std::fs::read_dir(home.join("profiles/default/trajectories"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|text| {
            text.lines()
                .filter(|l| l.contains("\"compression.committed\""))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Spec: context-compression, Gateway session hygiene, Overnight growth.
/// A conversation whose last answer pushed it past 85% of the window is
/// compacted by the daemon before the next inbound message runs.
#[test]
fn an_inbound_message_on_a_session_near_its_window_is_compacted_first() {
    // An 80K window with 40K reserved for output: the in-turn trigger is
    // 34K (85% of the input budget), hygiene compacts at 68K (85% of the
    // window). The second request (~8K plus the prompt) stays under the
    // in-turn trigger; its 62K answer takes the session past hygiene's.
    let rules = format!(
        "[[rule]]\nuser_regex = '^first$'\ntext = \"{}\"\n\n\
         [[rule]]\nuser_regex = '^second$'\ntext = \"{}\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"fast done\"\n",
        "early notes ".repeat(2_700),
        "long answer ".repeat(20_700),
    );
    let home = gateway_home_with(
        &rules,
        "\n[models.\"mock/mock\"]\ncontext_window = 80000\nmax_output_tokens = 40000\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    // The first three rows are protected from compaction: two short
    // exchanges open the conversation.
    for opener in ["hi", "how are things"] {
        send(home, "default", "u1", opener);
    }
    wait("both openers answered", Duration::from_mins(1), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.contains("fast done"))
            .count()
            == 2
    });
    send(home, "default", "u1", "first");
    wait_said(home, "default", "early notes");
    send(home, "default", "u1", "second");
    wait_said(home, "default", "long answer");
    assert!(compactions(home).is_empty(), "{:?}", compactions(home));
    send(home, "default", "u1", "third");
    wait("the third answer", Duration::from_mins(1), || {
        texts(home, "default")
            .iter()
            .filter(|t| t.contains("fast done"))
            .count()
            == 3
    });
    let committed = compactions(home);
    assert!(
        committed
            .first()
            .is_some_and(|c| c.contains("\"gateway_hygiene\"")),
        "{committed:?}"
    );
    daemon.stop(home);
}
