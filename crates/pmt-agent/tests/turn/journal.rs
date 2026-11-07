//! The turn journal and crash resume (spec: agent-turn-loop, Deterministic
//! turn journal; Crash resume mid-tool-batch).

use pmt_agent::turn::{ExitReason, outcome_unknown};
use pmt_store::{JournalKind as K, JournalRecord, NewMessage, Role, Source, Visibility};
use pmt_tools::{Status, ToolResult};
use serde_json::json;

use crate::support::{FakeTools, Profile, agent, mock_model, options};

const TOOL_THEN_ANSWER: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
step = 1
text = "Done."
"#;

#[tokio::test]
async fn a_turn_with_one_tool_call_journals_every_step_in_order() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let journal = profile.store.journal_of(s).unwrap();
    let kinds: Vec<K> = journal.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [
            K::TurnStarted,
            K::RequestSent,
            K::ResponseReceived,
            K::ToolStarted,
            K::ToolFinished,
            K::RequestSent,
            K::ResponseReceived,
            K::TurnFinished,
        ]
    );
    assert!(journal.iter().all(|r| r.turn_id == result.turn_id.as_str()));
    assert_eq!(journal[3].tool.as_deref(), Some("lookup"));
    assert_eq!(
        journal[3].idempotent,
        Some(false),
        "undeclared: not idempotent"
    );
    assert!(
        journal[1]
            .data
            .as_ref()
            .is_some_and(|h| h.starts_with("sha256:")),
        "request hash"
    );
    assert_eq!(journal[7].data.as_deref(), Some("completed"));
    assert_eq!(profile.store.unfinished_turn(s).unwrap(), None);
}

/// The state a process killed mid-batch leaves: the user message, the
/// assistant's four calls, `read_file` finished, `terminal` and
/// `web_search` started, the fourth never started.
fn crashed(profile: &Profile) -> pmt_core::id::SessionId {
    let s = profile.session(Source::Cli);
    let store = &profile.store;
    store
        .append_message(s, &NewMessage::new(Role::User, "deploy it"))
        .unwrap();
    let mut assistant = NewMessage::new(Role::Assistant, "");
    assistant.tool_calls = Some(json!([
        {"id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}"},
        {"id": "c2", "name": "terminal", "arguments": "{\"command\":\"make deploy\"}"},
        {"id": "c3", "name": "web_search", "arguments": "{\"query\":\"status\"}"},
        {"id": "c4", "name": "lookup", "arguments": "{\"q\":\"later\"}"},
    ]));
    store.append_message(s, &assistant).unwrap();
    let turn = "0190a8d2-0000-7000-8000-000000000001";
    let record = |kind| JournalRecord::new(turn, kind);
    let started = |call: &str, tool: &str, effect: &str, idempotent: bool| {
        let mut r = record(K::ToolStarted).with_call(call);
        r.tool = Some(tool.into());
        r.effect = Some(effect.into());
        r.idempotent = Some(idempotent);
        r
    };
    let read = ToolResult::text(Status::Ok, "contents of a (before the crash)");
    let blob = store.put_blob(&serde_json::to_vec(&read).unwrap()).unwrap();
    for r in [
        record(K::TurnStarted),
        record(K::RequestSent).with_data("h"),
        record(K::ResponseReceived),
        started("c1", "read_file", "none", false),
        started("c2", "terminal", "external", false),
        started("c3", "web_search", "external", true),
        record(K::ToolFinished)
            .with_call("c1")
            .with_data(blob.to_string()),
    ] {
        store.journal(s, &r).unwrap();
    }
    s
}

fn tool_rows(profile: &Profile, s: pmt_core::id::SessionId) -> Vec<(String, String)> {
    profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| (m.tool_call_id.unwrap(), m.content.unwrap_or_default()))
        .collect()
}

#[tokio::test]
async fn resuming_reuses_finished_results_reruns_safe_calls_and_marks_the_rest_unknown() {
    let profile = Profile::new("");
    let s = crashed(&profile);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let tools = FakeTools::new();
    let run = agent(&profile, s, model.clone(), tools.clone(), |_| {});

    let unfinished = run
        .agent
        .unfinished_turn()
        .unwrap()
        .expect("a crashed turn");
    assert_eq!(unfinished.interrupted_tools(), ["terminal", "web_search"]);
    let result = run
        .agent
        .resume_turn(Some(&profile.scope), unfinished, true, options())
        .await
        .unwrap();

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("Done."));
    let rows = tool_rows(&profile, s);
    let by_id = |id: &str| rows.iter().find(|(c, _)| c == id).unwrap().1.clone();
    assert_eq!(by_id("c1"), "contents of a (before the crash)", "reused");
    assert_eq!(by_id("c2"), outcome_unknown("terminal"));
    let ran: Vec<String> = tools
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| format!("{} {}", c.name, c.arguments))
        .collect();
    assert_eq!(
        ran,
        [
            "web_search {\"query\":\"status\"}",
            "lookup {\"q\":\"later\"}"
        ],
        "the idempotent call re-ran with the same arguments, the unstarted one ran"
    );
    assert_eq!(
        model.requests().len(),
        1,
        "the loop went on with the next request"
    );
    assert_eq!(profile.store.unfinished_turn(s).unwrap(), None);
}

#[tokio::test]
async fn declining_closes_the_turn_as_crashed_with_a_boundary_row() {
    let profile = Profile::new("");
    let s = crashed(&profile);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let tools = FakeTools::new();
    let run = agent(&profile, s, model.clone(), tools.clone(), |_| {});
    let unfinished = run.agent.unfinished_turn().unwrap().unwrap();
    let result = run
        .agent
        .resume_turn(Some(&profile.scope), unfinished, false, options())
        .await
        .unwrap();

    assert_eq!(result.exit_reason, ExitReason::Crashed);
    assert_eq!(tools.calls(), 0, "nothing runs");
    assert!(model.requests().is_empty(), "no model call");
    let last = profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert!(
        last.content
            .unwrap()
            .starts_with("[prometheus] The turn ended (crashed)"),
    );
    assert_eq!(tool_rows(&profile, s).len(), 4, "every call is answered");
    assert_eq!(profile.store.unfinished_turn(s).unwrap(), None);
}

#[tokio::test]
async fn a_new_message_closes_a_crashed_turn_first() {
    let profile = Profile::new("");
    let s = crashed(&profile);
    let model = mock_model(&profile, "[[rule]]\ntext = \"Fresh answer.\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "never mind").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    let rows = profile.store.messages(s, Visibility::Active).unwrap();
    let roles: Vec<Role> = rows.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Tool,
            Role::Tool,
            Role::Tool,
            Role::Assistant,
            Role::User,
            Role::Assistant
        ]
    );
}
