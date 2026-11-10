//! Comparing a replay with its recording (spec: replay-and-evaluation,
//! Divergence detection): the unified diff of two canonical forms, and
//! the agent-loop view of a trajectory with ids and timestamps removed.

use std::fmt::Write as _;

use serde_json::Value;
use similar::TextDiff;

/// The longest divergence diff shown.
pub const MAX_DIFF_LINES: usize = 200;

/// The trajectory events a strict replay reproduces: the agent loop's own
/// (approvals, hooks, subagents and the like happen inside tools, whose
/// results a strict replay serves instead of running them).
const LOOP_EVENTS: &[&str] = &[
    "turn.start",
    "turn.step",
    "turn.end",
    "user.message",
    "assistant.message",
    "assistant.reasoning",
    "model.call",
    "tool.call",
    "tool.result",
    "tool.batch_planned",
    "compression.started",
    "compression.committed",
    "compression.failed",
    "budget.warning",
    "budget.exceeded",
];

/// Keys that differ between two runs of the same session by construction.
fn volatile(key: &str) -> bool {
    key == "id"
        || key.ends_with("_id")
        || matches!(
            key,
            "ts" | "seq"
                | "latency_ms"
                | "duration_ms"
                | "wall_ms"
                | "cache_hit"
                | "cost_usd"
                | "cost_status"
        )
}

fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !volatile(key));
            map.values_mut().for_each(strip);
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// The agent-loop events of a trajectory (JSON lines), each as
/// `{type, payload}` without ids, timestamps or timings.
#[must_use]
pub fn loop_events(trajectory: &str) -> Vec<Value> {
    trajectory
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| {
            line["type"]
                .as_str()
                .is_some_and(|kind| LOOP_EVENTS.contains(&kind))
        })
        .map(|line| {
            let mut event = serde_json::json!({"type": line["type"], "payload": line["payload"]});
            strip(&mut event);
            event
        })
        .collect()
}

/// `value` as indented JSON with sorted keys, one field per line, so a
/// diff points at the lines that changed.
#[must_use]
pub fn pretty(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&map[k])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    let mut text = serde_json::to_string_pretty(&sorted(value)).unwrap_or_default();
    // Multi-line strings (prompts) are compared line by line.
    text = text.replace("\\n", "\n");
    text.push('\n');
    text
}

/// The unified diff of `recorded` and `replayed`, cut to
/// [`MAX_DIFF_LINES`] lines.
#[must_use]
pub fn unified(recorded: &str, replayed: &str) -> String {
    let diff = TextDiff::from_lines(recorded, replayed)
        .unified_diff()
        .context_radius(3)
        .header("recorded", "replayed")
        .to_string();
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return diff;
    }
    let mut cut = lines[..MAX_DIFF_LINES].join("\n");
    let more = lines.len() - MAX_DIFF_LINES;
    let _ = write!(cut, "\n... ({more} more lines)\n");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_timestamps_and_tool_internals_are_left_out() {
        let trajectory = [
            r#"{"v":"1.0","ts":"t1","seq":1,"session_id":"a","type":"user.message","payload":{"message_id":"m1","text":"hi"}}"#,
            r#"{"v":"1.0","ts":"t2","seq":2,"session_id":"a","type":"approval.request","payload":{"x":1}}"#,
            r#"{"v":"1.0","ts":"t3","seq":3,"session_id":"a","type":"model.call","payload":{"call_id":"c","latency_ms":9,"model":"m"}}"#,
        ]
        .join("\n");
        let events = loop_events(&trajectory);
        assert_eq!(
            events,
            [
                serde_json::json!({"type": "user.message", "payload": {"text": "hi"}}),
                serde_json::json!({"type": "model.call", "payload": {"model": "m"}}),
            ]
        );
    }

    #[test]
    fn long_diffs_are_cut() {
        let lines = |tag: char| -> String {
            (0..500)
                .flat_map(|i| [tag.to_string(), i.to_string(), "\n".to_owned()])
                .collect()
        };
        let (a, b) = (lines('a'), lines('b'));
        let diff = unified(&a, &b);
        assert!(diff.lines().count() <= MAX_DIFF_LINES + 1);
        assert!(diff.contains("more lines"));
        let small = unified("x\ny\n", "x\nz\n");
        assert!(small.contains("-y") && small.contains("+z"));
    }
}
