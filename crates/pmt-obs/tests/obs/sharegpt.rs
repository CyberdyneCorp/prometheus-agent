//! `ShareGPT` export (spec: observability, `ShareGPT` trajectory export): a
//! recorded session renders to the golden record byte for byte.

use pmt_obs::sharegpt::{completed, record};
use pmt_obs::trajectory::Line;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{
    MessageText, Reasoning, SessionStart, ToolCall, ToolResult, TurnEnd, TurnStart, Usage,
};
use serde_json::json;

fn line(seq: u64, event: &Event) -> Line {
    Line {
        v: "1.0".to_owned(),
        ts: format!("2026-11-01T10:00:{seq:02}.000000Z"),
        seq,
        session_id: "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W".to_owned(),
        root_session_id: "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W".to_owned(),
        turn_id: (seq > 1).then(|| "turn_1".to_owned()),
        span_id: format!("{seq:016x}"),
        parent_span_id: None,
        kind: event.kind().to_owned(),
        payload: event.payload(),
    }
}

fn session(end: &str) -> Vec<Line> {
    vec![
        line(
            1,
            &Event::SessionStart(SessionStart {
                source: "cli".to_owned(),
                title: None,
                model: Some("mock/scripted".to_owned()),
                parent_session_id: None,
                trace_id: None,
            }),
        ),
        line(2, &Event::TurnStart(TurnStart { message_id: None })),
        line(
            3,
            &Event::UserMessage(MessageText {
                message_id: None,
                text: "What does notes.txt say?".to_owned(),
            }),
        ),
        line(
            4,
            &Event::AssistantReasoning(Reasoning {
                message_id: None,
                text: Some("I should read the file first.".to_owned()),
                items: None,
            }),
        ),
        line(
            5,
            &Event::ToolCall(ToolCall {
                call_id: "call_1".to_owned(),
                tool: "read_file".to_owned(),
                arguments: json!({"path": "notes.txt"}),
            }),
        ),
        line(
            6,
            &Event::ToolResult(ToolResult {
                call_id: "call_1".to_owned(),
                tool: "read_file".to_owned(),
                ok: true,
                duration_ms: Some(2),
                output: Some("buy milk".to_owned()),
                error: None,
            }),
        ),
        line(
            7,
            &Event::AssistantMessage(MessageText {
                message_id: None,
                text: "It says: buy milk.".to_owned(),
            }),
        ),
        line(
            8,
            &Event::TurnEnd(TurnEnd {
                reason: end.to_owned(),
                iterations: 2,
                usage: Usage::default(),
                cache_hit_rate: None,
                context_tokens: None,
                compaction_trigger_tokens: None,
            }),
        ),
    ]
}

/// Scenario: Tool call rendering.
#[test]
fn a_tool_using_session_renders_to_the_golden_sharegpt_record() {
    let tools = vec![
        json!({"name": "read_file", "description": "Read a file", "parameters": {"type": "object"}}),
    ];
    let exported = record(
        &session("completed"),
        Some("You are Prometheus."),
        tools,
        vec!["file".to_owned()],
    );
    let text = serde_json::to_string(&exported).unwrap() + "\n";
    let golden = include_str!("golden/sharegpt.jsonl");
    assert_eq!(text, golden);
    // A standard JSON-lines loader reads it: one object per line.
    let parsed: serde_json::Value = serde_json::from_str(golden.lines().next().unwrap()).unwrap();
    assert_eq!(parsed["conversations"][2]["from"], "gpt");
    assert!(parsed["conversations"][2]["value"].as_str().unwrap().contains("<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"notes.txt\"}}</tool_call>"));
    assert_eq!(parsed["conversations"][3]["from"], "tool");
}

#[test]
fn a_session_whose_last_turn_failed_is_not_completed() {
    assert!(completed(&session("completed")));
    assert!(!completed(&session("provider_error")));
}
