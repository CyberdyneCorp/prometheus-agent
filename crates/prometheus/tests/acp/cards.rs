//! Task 66.3 fault injection, part 2: fail-closed approval and clarify
//! bridging, edit-approval modes, validated config options, model
//! switching and the ACP toolset (spec: acp-adapter).

use std::time::{Duration, Instant};

use pmt_rpc::contracts::{AttachParams, AttachRole, SessionAttach};
use serde_json::{Value, json};

use super::common::{self, daemon};
use super::fixture::{Acp, Home};

/// A mock script whose model runs `command` once, then answers `after`.
fn command_script(command: &str) -> String {
    format!(
        "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nid = \"call-cmd\"\nname = \"terminal\"\narguments = {{ command = \"{command}\" }}\n\n[[rule]]\ntext = \"after\"\nrepeat = true\n"
    )
}

fn editor(home: &Home) -> (Acp, String) {
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    (acp, session)
}

fn tools_of(home: &Home, session: &str) -> String {
    common::ok(&common::run(
        home.path(),
        &["tools", "list", "--session", session],
    ))
}

fn exposed(listing: &str, tool: &str) -> bool {
    listing
        .lines()
        .any(|l| l.split_whitespace().next() == Some(tool) && l.contains(" exposed"))
}

/// The status the editor last saw for tool call `id`.
fn call_status(acp: &Acp, session: &str, id: &str) -> Option<String> {
    acp.updates(session)
        .iter()
        .rev()
        .find(|u| u["toolCallId"] == id && u["status"].is_string())
        .map(|u| u["status"].as_str().unwrap().to_owned())
}

/// Scenario: Dismissed card, editor is the only driver.
#[test]
fn a_dismissed_card_denies_at_once_when_the_editor_is_the_only_driver() {
    let home = Home::new();
    home.config("\n[approvals]\ntimeout_secs = 300\n");
    home.script(&command_script("rm -rf build/"));
    std::fs::create_dir_all(home.work.join("build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let started = Instant::now();
    let id = acp.send_prompt(&session, "go");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(
        started.elapsed() < Duration::from_mins(1),
        "waited for the deadline"
    );
    let cards = acp.requests("session/request_permission");
    assert_eq!(cards.len(), 1, "{cards:?}");
    let options: Vec<&str> = cards[0]["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(
        options,
        ["allow_once", "allow_session", "allow_always", "reject_once"]
    );
    assert_eq!(
        call_status(&acp, &session, "call-cmd").as_deref(),
        Some("failed")
    );
    assert!(home.work.join("build").exists(), "denied: nothing ran");
}

/// Scenario: Dismissed card, desktop also attached.
#[test]
fn a_dismissed_card_leaves_the_approval_to_the_desktop() {
    let home = Home::new();
    home.script(&command_script("rm -rf build/"));
    std::fs::create_dir_all(home.work.join("build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (desktop, mut inbox) = runtime.block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &[]).await;
        let attach = AttachParams {
            session_id: session.clone(),
            role: AttachRole::Driver,
            last_seen_seq: None,
            epoch: None,
        };
        client.call_typed::<SessionAttach>(&attach).await.unwrap();
        (client, inbox)
    });
    let id = acp.send_prompt(&session, "go");
    acp.wait_for("the editor's card", |f| {
        f["method"] == "session/request_permission"
    });
    acp.pump(Duration::from_millis(500));
    let request = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), inbox.requests.recv())
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(request.method, "approval.request");
    desktop.respond(&request.id, Ok(json!({"decision": "once"})));
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert_eq!(
        call_status(&acp, &session, "call-cmd").as_deref(),
        Some("completed")
    );
    assert!(
        !home.work.join("build").exists(),
        "approved from the desktop"
    );
}

/// Edit-approval modes: `default` asks with a diff card, `accept_edits`
/// writes inside the workspace without asking; an unknown mode is
/// `-32602`.
#[test]
fn edits_ask_with_a_diff_in_default_mode_and_not_in_accept_edits() {
    let home = Home::new();
    home.script(
        "[[rule]]\nuser = \"write one\"\n[[rule.tool_calls]]\nname = \"patch\"\narguments = { path = \"notes.txt\", old_string = \"old text\", new_string = \"new text\" }\n\n\
         [[rule]]\nuser = \"write two\"\n[[rule.tool_calls]]\nname = \"patch\"\narguments = { path = \"notes.txt\", old_string = \"old text\", new_string = \"new text\" }\n\n\
         [[rule]]\ntext = \"written\"\nrepeat = true\n",
    );
    std::fs::write(home.work.join("notes.txt"), "old text").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    acp.respond_with(Box::new(|request: &Value| {
        if request["method"] == "session/request_permission" {
            Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}))
        } else {
            Err(json!({"code": -32601, "message": "not offered"}))
        }
    }));
    let id = acp.send_prompt(&session, "write one");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let card = acp
        .requests("session/request_permission")
        .pop()
        .expect("an edit card");
    let diff = card["params"]["toolCall"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "diff")
        .cloned()
        .expect("a diff");
    assert_eq!(diff["oldText"], "old text");
    assert_eq!(diff["newText"], "new text");
    assert_eq!(
        std::fs::read_to_string(home.work.join("notes.txt")).unwrap(),
        "new text",
        "{:?}",
        acp.updates(&session)
    );
    let unknown = acp.call(
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "yolo"}),
    );
    assert_eq!(unknown["error"]["code"], -32602);
    acp.ok(
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "accept_edits"}),
    );
    std::fs::write(home.work.join("notes.txt"), "old text").unwrap();
    let before = acp.requests("session/request_permission").len();
    let id = acp.send_prompt(&session, "write two");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert_eq!(
        acp.requests("session/request_permission").len(),
        before,
        "no card"
    );
    assert_eq!(
        std::fs::read_to_string(home.work.join("notes.txt")).unwrap(),
        "new text"
    );
}

/// Scenario: Mode changed in the desktop; a mode change while a turn runs
/// is refused.
#[test]
fn a_mode_change_elsewhere_reaches_the_editor_and_busy_changes_are_refused() {
    let home = Home::new();
    home.script("latency_ms = 300\n[[rule]]\nuser = \"slow\"\ntext = \"slow answer here\"\n\n[[rule]]\ntext = \"quick\"\nrepeat = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    home.rpc(
        "session.configure",
        &json!({"session_id": session, "edit_mode": "accept_edits"}),
    );
    acp.wait_for("the mode update", |f| {
        f["params"]["update"]["sessionUpdate"] == "current_mode_update"
            && f["params"]["update"]["currentModeId"] == "accept_edits"
    });
    let id = acp.send_prompt(&session, "slow");
    acp.wait_for("the turn's chunks", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    let busy = acp.call(
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "dont_ask"}),
    );
    assert_eq!(
        busy["error"]["message"],
        "Mode changes apply when the session is idle; try again after this turn."
    );
    acp.response(id);
}

/// Scenario: Unknown option id.
#[test]
fn config_options_are_validated_and_applied() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let unknown = acp.call(
        "session/set_config_option",
        &json!({"sessionId": session, "configId": "temperature", "value": "0.2"}),
    );
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(
        unknown["error"]["message"],
        "unknown config option temperature"
    );
    let invalid = acp.call(
        "session/set_config_option",
        &json!({"sessionId": session, "configId": "reasoning_effort", "value": "loud"}),
    );
    assert_eq!(invalid["error"]["code"], -32602);
    assert!(
        invalid["error"]["message"]
            .as_str()
            .unwrap()
            .contains("allowed: none, minimal, low")
    );
    let set = acp.ok(
        "session/set_config_option",
        &json!({"sessionId": session, "configId": "edit_approval_policy", "value": "dont_ask"}),
    );
    let policy = set["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "edit_approval_policy")
        .unwrap()
        .clone();
    assert_eq!(policy["currentValue"], "dont_ask");
}

/// Scenario: Switch during a turn.
#[test]
fn a_model_switch_during_a_turn_applies_from_the_next() {
    let home = Home::new();
    home.config("\n[model.aliases.second]\nprovider = \"mock\"\nmodel = \"mock-second\"\n");
    home.script("latency_ms = 300\n[[rule]]\nuser = \"slow\"\ntext = \"slow answer here\"\n\n[[rule]]\ntext = \"quick\"\nrepeat = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    let id = acp.send_prompt(&session, "slow");
    acp.wait_for("the turn's chunks", |f| {
        f["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });
    let unqualified = acp.call(
        "session/set_model",
        &json!({"sessionId": session, "modelId": "mock-second"}),
    );
    assert_eq!(unqualified["error"]["code"], -32602);
    acp.ok(
        "session/set_model",
        &json!({"sessionId": session, "modelId": "mock/mock-second"}),
    );
    assert!(
        acp.agent_text(&session)
            .contains("Model change applies from the next turn.")
    );
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let settings = home.rpc("session.settings", &json!({"session_id": session}));
        if settings["model"] == "mock-second" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the switch never applied: {settings}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Scenario: Default ACP session.
#[test]
fn an_acp_session_has_clarify_and_no_messaging_send_tools() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let (_acp, session) = editor(&home);
    let listing = tools_of(&home, &session);
    assert!(exposed(&listing, "clarify"), "{listing}");
    assert!(!exposed(&listing, "send_message"), "{listing}");
    assert!(exposed(&listing, "terminal"), "{listing}");
}

/// Clarify without elicitation: one card per question, its choices plus
/// `Skip`; the picked choice is the answer.
#[test]
fn clarify_questions_become_cards_with_their_choices_and_skip() {
    let home = Home::new();
    home.script(
        "[[rule]]\nuser = \"ask me\"\n[[rule.tool_calls]]\nname = \"clarify\"\narguments = { questions = [{ question = \"Which color?\", choices = [\"red\", \"blue\"] }] }\n\n[[rule]]\ntool_result = \"blue\"\ntext = \"you chose blue\"\n\n[[rule]]\ntext = \"no answer\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let (mut acp, session) = editor(&home);
    acp.respond_with(Box::new(|request: &Value| {
        let options = request["params"]["options"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let blue = options.iter().find(|o| o["name"] == "blue").cloned();
        match blue {
            Some(option) => {
                Ok(json!({"outcome": {"outcome": "selected", "optionId": option["optionId"]}}))
            }
            None => Ok(json!({"outcome": {"outcome": "cancelled"}})),
        }
    }));
    let id = acp.send_prompt(&session, "ask me");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let card = acp
        .requests("session/request_permission")
        .pop()
        .expect("a question card");
    let names: Vec<&str> = card["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["red", "blue", "Skip"]);
    assert!(
        acp.agent_text(&session).contains("you chose blue"),
        "{}",
        acp.agent_text(&session)
    );
}
