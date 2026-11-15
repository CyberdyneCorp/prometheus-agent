//! Task 66.3 fault injection, part 1: prompt submission exactly once, the
//! terminal outcome, scoped cancel, an unknown outcome after a daemon
//! restart, slash commands, reply projection and terminal tool statuses
//! (spec: acp-adapter).

use std::time::Duration;

use base64::Engine as _;
use pmt_rpc::contracts::{AttachParams, AttachRole, PromptSubmit, SessionAttach, SubmitParams};
use serde_json::{Value, json};

use super::common::{self, daemon};
use super::fixture::{Acp, Home};

/// What an admission in flight answers when the daemon connection is lost.
const GATEWAY_LOST: &str = "Gateway disconnected; turn outcome is unknown. Load the session to check whether it completed; do not resend the work.";

/// A connected editor with a new session.
fn editor(home: &Home) -> (Acp, String) {
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    (acp, session)
}

/// Another surface (a CLI client) driving `session`, and its turn's id.
fn other_surface_turn(
    home: &Home,
    session: &str,
    text: &str,
) -> (
    tokio::runtime::Runtime,
    pmt_rpc::client::Client,
    pmt_rpc::client::Inbox,
    String,
) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (client, inbox, turn) = runtime.block_on(async {
        let (client, inbox) = daemon::connect(home.path(), "default").await;
        let attach = AttachParams {
            session_id: session.to_owned(),
            role: AttachRole::Driver,
            last_seen_seq: None,
            epoch: None,
        };
        client.call_typed::<SessionAttach>(&attach).await.unwrap();
        let submit = SubmitParams {
            session_id: session.to_owned(),
            text: text.to_owned(),
            images: Vec::new(),
            files: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let submitted = client.call_typed::<PromptSubmit>(&submit).await.unwrap();
        (client, inbox, submitted.turn_id)
    });
    (runtime, client, inbox, turn)
}

/// The `turn.finished` of `turn` on `inbox`.
fn finished(
    runtime: &tokio::runtime::Runtime,
    inbox: &mut pmt_rpc::client::Inbox,
    turn: &str,
) -> Value {
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                let event = inbox.events.recv().await.expect("connection open");
                if event.event == "turn.finished" && event.data["turn_id"] == turn {
                    return event.data;
                }
            }
        })
        .await
        .expect("the turn finished")
    })
}

/// Scenario: Turn fails.
#[test]
fn a_failed_turn_is_an_error_and_the_next_prompt_runs() {
    let home = Home::new();
    home.script(
        "[[rule]]\nuser = \"please fail\"\nfault = \"auth_error\"\n\n[[rule]]\ntext = \"recovered\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "please fail");
    let failed = acp.response(id);
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("admitted_turn_failed: "), "{failed}");
    let id = acp.send_prompt(&session, "again");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(acp.agent_text(&session).contains("recovered"));
}

/// Scenario: Final text after streaming, end to end: the streamed chunks
/// and the final message add up to the answer exactly once.
#[test]
fn the_answer_is_shown_exactly_once() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "say it once");
    acp.response(id);
    assert_eq!(acp.agent_text(&session), "mock: say it once");
}

/// Scenario: Interrupted tool; cancelling the editor's own admission
/// stops its turn: `cancelled`, and the running terminal call `failed`.
#[test]
fn cancelling_a_running_command_fails_its_tool_call() {
    let home = Home::new();
    home.script(
        "[[rule]]\nuser = \"run it\"\n[[rule.tool_calls]]\nid = \"call-sleep\"\nname = \"terminal\"\narguments = { command = \"sleep 30\" }\n\n[[rule]]\ntext = \"done\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "run it");
    acp.wait_for("the terminal call", |f| {
        f["params"]["update"]["sessionUpdate"] == "tool_call"
    });
    acp.notify("session/cancel", &json!({"sessionId": session}));
    assert_eq!(acp.response(id)["result"]["stopReason"], "cancelled");
    let failed = acp
        .updates(&session)
        .into_iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "call-sleep")
        .expect("the call reached a terminal status");
    assert_eq!(failed["status"], "failed");
}

/// Scenario: TUI turn running. A turn another surface submitted keeps
/// running when the editor cancels.
#[test]
fn cancel_never_stops_another_surfaces_turn() {
    let home = Home::new();
    home.script("latency_ms = 300\n[[rule]]\ntext = \"the tui answer\"\nrepeat = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let (runtime, _client, mut inbox, turn) = other_surface_turn(&home, &session, "from the tui");
    acp.wait_for("the other turn's chunks", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    acp.notify("session/cancel", &json!({"sessionId": session}));
    let end = finished(&runtime, &mut inbox, &turn);
    assert_eq!(end["exit_reason"], "completed", "{end}");
}

/// Scenario: Resend after a lost ack. The first prompt's acknowledgement
/// is lost with the connection (fault injection); the editor resends the
/// same prompt with its image: one admission, one staged image.
#[test]
fn a_resent_prompt_after_a_lost_ack_runs_once() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[("PMT_TEST_ACP_DROP_ACK", "1")]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let image = base64::engine::general_purpose::STANDARD.encode(common::PNG);
    let prompt = json!({"sessionId": session, "prompt": [
        {"type": "text", "text": "look at this once"},
        {"type": "image", "mimeType": "image/png", "data": image}
    ]});
    let lost = acp.call("session/prompt", &prompt);
    assert_eq!(lost["error"]["message"], GATEWAY_LOST);
    let resent = acp.call("session/prompt", &prompt);
    assert_eq!(resent["result"]["stopReason"], "end_turn", "{resent}");
    let transcript = home.rpc("session.transcript", &json!({"session_id": session}));
    let prompts = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(prompts, 1, "{transcript}");
    let blobs = common::files_under(&home.path().join("profiles/default/blobs"));
    let images = blobs
        .iter()
        .filter(|p| std::fs::read(p).is_ok_and(|b| b == common::PNG))
        .count();
    assert_eq!(images, 1, "{blobs:?}");
}

/// Scenario: Restart mid-turn. The daemon dies during the turn: the
/// prompt's outcome is unknown at once; once the restarted daemon has
/// recorded the crashed turn, re-attaching shows its outcome and new
/// work is accepted.
#[cfg(unix)]
#[test]
fn a_daemon_restart_fails_the_prompt_and_reattach_shows_the_outcome() {
    let home = Home::new();
    home.script("latency_ms = 500\n[[rule]]\ntext = \"a long answer that will not finish\"\nrepeat = true\n");
    let running = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "start something long");
    acp.wait_for("the turn's chunks", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    let pid = running.pid.to_string();
    std::mem::forget(running);
    std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap();
    let lost = acp.response(id);
    assert_eq!(lost["error"]["message"], GATEWAY_LOST, "{lost}");
    let _daemon = daemon::start(home.path(), &[]);
    home.script("[[rule]]\ntext = \"fresh answer\"\nrepeat = true\n");
    let id = acp.send_prompt(&session, "new work");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let transcript = home.rpc("session.transcript", &json!({"session_id": session}));
    assert!(
        transcript.to_string().contains("The turn ended (crashed)"),
        "the outcome the re-attach found: {transcript}"
    );
}

/// Scenario: Restart mid-turn, outcome unknown. An admission whose
/// acknowledgement was lost with the connection (fault injection) has
/// no known outcome: new work is refused naming `/discard <id>`, which
/// drops it.
#[test]
fn an_unknown_outcome_refuses_new_work_until_discarded() {
    let home = Home::new();
    home.script("latency_ms = 300\n[[rule]]\nuser = \"long\"\ntext = \"a long answer that keeps going on\"\n\n[[rule]]\ntext = \"short answer\"\nrepeat = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[("PMT_TEST_ACP_DROP_ACK", "1")]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let id = acp.send_prompt(&session, "something long");
    assert_eq!(acp.response(id)["error"]["message"], GATEWAY_LOST);
    let refused = acp.call(
        "session/prompt",
        &json!({"sessionId": session, "prompt": [{"type": "text", "text": "new work"}]}),
    );
    let message = refused["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(message.starts_with("unknown_execution"), "{refused}");
    let discard = message
        .split("send ")
        .nth(1)
        .and_then(|rest| rest.split(" to drop").next())
        .expect("the refusal names /discard <id>")
        .to_owned();
    assert!(discard.starts_with("/discard "), "{message}");
    let id = acp.send_prompt(&session, &discard);
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(acp.agent_text(&session).contains("Discarded admission"));
    let id = acp.send_prompt(&session, "new work");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
}

/// Scenario: Path at line start.
#[test]
fn a_path_at_line_start_is_prompt_text() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "/var/log/x is wrong");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(&session), "mock: /var/log/x is wrong");
}

/// Scenario: Refused command. `/compress` while another surface's turn
/// runs: the refusal is shown and the prompt still ends normally.
#[test]
fn a_refused_command_is_shown_and_ends_the_prompt_normally() {
    let home = Home::new();
    home.script("latency_ms = 400\n[[rule]]\ntext = \"busy for a while yet\"\nrepeat = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let advertised = acp.wait_for("the commands", |f| {
        f["params"]["update"]["sessionUpdate"] == "available_commands_update"
    });
    let names: Vec<&str> = advertised["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"compress"), "{names:?}");
    assert!(
        !names.contains(&"exit"),
        "client-side commands are not advertised"
    );
    let (runtime, _client, mut inbox, turn) = other_surface_turn(&home, &session, "keep busy");
    acp.wait_for("the other turn's chunks", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    let id = acp.send_prompt(&session, "/compress");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(
        acp.agent_text(&session)
            .contains("/compress is unavailable while the agent is working"),
        "{}",
        acp.agent_text(&session)
    );
    finished(&runtime, &mut inbox, &turn);
}

/// A command's notice is shown as an agent message.
#[test]
fn a_command_answer_is_an_agent_message() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "/version");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(
        acp.agent_text(&session).contains("prometheus "),
        "{:?}",
        acp.seen
    );
}
