//! What a task's trajectory says: its tool uses (the answer-free
//! boundary scans them), whether the run's budget refused a call, and the
//! batch statistics' view of it.

use pmt_eval::batch::{ModelUse, Trace};
use pmt_eval::boundary::ToolUse;
use serde_json::Value;

/// A trajectory as JSON values (`type`, `payload`, ...).
pub type Lines = Vec<Value>;

fn kind(line: &Value) -> &str {
    line["type"].as_str().unwrap_or_default()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Every tool call and result, for the boundary scanner.
#[must_use]
pub fn tool_uses(lines: &[Value]) -> Vec<ToolUse> {
    lines
        .iter()
        .filter_map(|line| {
            let payload = &line["payload"];
            let tool = text(&payload["tool"]);
            match kind(line) {
                "tool.call" => Some(ToolUse {
                    tool,
                    arguments: text(&payload["arguments"]),
                    result: String::new(),
                }),
                "tool.result" => Some(ToolUse {
                    tool,
                    arguments: String::new(),
                    result: format!("{}{}", text(&payload["output"]), text(&payload["error"])),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Whether the run's budget (the eval root's `root` scope) refused a call.
#[must_use]
pub fn root_budget_refused(lines: &[Value]) -> bool {
    lines
        .iter()
        .any(|l| kind(l) == "budget.exceeded" && l["payload"]["scope"] == "root")
}

/// The batch statistics' view of a task.
#[must_use]
pub fn batch_trace(task_id: &str, ok: bool, hit_max_turns: bool, lines: &[Value]) -> Trace {
    let mut trace = Trace {
        task_id: task_id.to_owned(),
        ok,
        hit_max_turns,
        ..Trace::default()
    };
    let mut reasoning_pending = false;
    for line in lines {
        let payload = &line["payload"];
        match kind(line) {
            "assistant.reasoning" => reasoning_pending = true,
            "assistant.message" => {
                trace.assistant_turns += 1;
                if std::mem::take(&mut reasoning_pending) {
                    trace.reasoning_turns += 1;
                }
            }
            "tool.result" => trace.tool_calls.push((
                text(&payload["tool"]),
                payload["ok"].as_bool() == Some(true),
            )),
            "model.call" => trace.calls.push(ModelUse {
                model: format!("{}/{}", text(&payload["provider"]), text(&payload["model"])),
                input_tokens: payload["tokens"]["input"].as_u64().unwrap_or_default(),
                output_tokens: payload["tokens"]["output"].as_u64().unwrap_or_default(),
                cost_usd: payload["cost_usd"].as_f64(),
            }),
            _ => {}
        }
    }
    trace
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_trajectory_yields_tool_uses_budget_stops_and_counts() {
        let lines = vec![
            json!({"type": "assistant.reasoning", "payload": {}}),
            json!({"type": "assistant.message", "payload": {}}),
            json!({"type": "tool.call", "payload": {"tool": "terminal", "arguments": {"command": "ls"}}}),
            json!({"type": "tool.result", "payload": {"tool": "terminal", "ok": false, "error": {"message": "x"}}}),
            json!({"type": "model.call", "payload": {"provider": "mock", "model": "mock", "tokens": {"input": 5, "output": 2}, "cost_usd": null}}),
            json!({"type": "budget.exceeded", "payload": {"scope": "root"}}),
        ];
        assert!(root_budget_refused(&lines));
        assert_eq!(tool_uses(&lines).len(), 2);
        let trace = batch_trace("t", true, false, &lines);
        assert_eq!((trace.assistant_turns, trace.reasoning_turns), (1, 1));
        assert_eq!(trace.tool_calls, [("terminal".to_owned(), false)]);
        assert_eq!(trace.calls[0].cost_usd, None);
    }
}
