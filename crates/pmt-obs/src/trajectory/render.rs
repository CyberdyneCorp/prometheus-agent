//! The canonical text rendering of a trajectory (spec: observability,
//! Canonical text rendering). It is deterministic, the same lines always
//! give byte-identical text, and it is the one renderer used by
//! `prometheus trajectory show`, harness evolution, selection and eval.
//!
//! Each rendered event is one block, blocks are separated by a blank line:
//! `[USER]`, `[ASSISTANT]`, `[THINKING]` and `[OBSERVATION]` put their text
//! on the lines after the marker; `[ACTION] <tool>(<args>)` and the other
//! markers are one line. Events without a marker (`turn.start`,
//! `model.call`...) are not rendered; event types this build does not know
//! render as `[EVENT <type>]`.

use pmt_core::error::Error;
use serde_json::Value;

use super::payload::{ToolCall, ToolResult};
use super::{Decoded, Event, Line};

/// Default clip of tool arguments, in characters.
pub const ARGS_TRUNC: usize = 2_000;
/// Default clip of each observation, in characters.
pub const OBS_TRUNC: usize = 50_000;
/// Observations are never clipped below this, whatever `--obs-trunc` says.
pub const MIN_OBS_TRUNC: usize = 10_000;

/// Rendering limits (`--args-trunc`, `--obs-trunc`, `--max-chars`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// Clip of tool arguments.
    pub args_trunc: usize,
    /// Clip of each observation (at least [`MIN_OBS_TRUNC`]).
    pub obs_trunc: usize,
    /// Clip of the whole output; `None` is unlimited.
    pub max_chars: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            args_trunc: ARGS_TRUNC,
            obs_trunc: OBS_TRUNC,
            max_chars: None,
        }
    }
}

/// Event types that render as a block; a payload of one of these left out
/// by the size limit still shows where it was.
const RENDERED: &[&str] = &[
    "user.message",
    "assistant.message",
    "assistant.reasoning",
    "tool.call",
    "tool.result",
    "approval.request",
    "approval.decision",
    "subagent.spawned",
    "subagent.progress",
    "subagent.finished",
    "compression.committed",
    "compression.failed",
    "harness.note",
    "harness.activated",
    "error",
];

/// Renders `lines` as canonical text.
///
/// # Errors
///
/// `trajectory.malformed` when a known event's payload does not match its
/// schema.
pub fn render(lines: &[Line], options: &RenderOptions) -> Result<String, Error> {
    let mut blocks = Vec::new();
    for line in lines {
        let block = match line.decode()? {
            Decoded::Known(event) => known(&event, options),
            Decoded::Unknown => Some(format!("[EVENT {}]", line.kind)),
            Decoded::Omitted { bytes } => RENDERED
                .contains(&line.kind.as_str())
                .then(|| format!("[EVENT {}] payload omitted ({bytes} bytes)", line.kind)),
        };
        blocks.extend(block);
    }
    let mut text = blocks.join("\n\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Ok(match options.max_chars {
        Some(max) => clip(&text, max),
        None => text,
    })
}

fn known(event: &Event, options: &RenderOptions) -> Option<String> {
    let block = match event {
        Event::UserMessage(m) => format!("[USER]\n{}", m.text),
        Event::AssistantMessage(m) if m.text.is_empty() => return None,
        Event::AssistantMessage(m) => format!("[ASSISTANT]\n{}", m.text),
        Event::AssistantReasoning(r) => format!("[THINKING]\n{}", r.text.as_deref()?),
        Event::ToolCall(call) => action(call, options.args_trunc),
        Event::ToolResult(result) => observation(result, options.obs_trunc.max(MIN_OBS_TRUNC)),
        Event::ApprovalRequest(r) => format!("[APPROVAL] {}: {}", r.tool, r.summary),
        Event::ApprovalDecision(d) => format!("[APPROVAL] {} by {}", d.decision, d.by),
        Event::SubagentSpawned(s) => format!("[SUBAGENT {}] spawned: {}", s.subagent_id, s.goal),
        Event::SubagentProgress(p) => format!("[SUBAGENT {}] {}", p.subagent_id, p.activity),
        Event::SubagentFinished(f) => {
            format!("[SUBAGENT {}] {}: {}", f.subagent_id, f.status, f.summary)
        }
        Event::CompressionCommitted(c) => format!(
            "[COMPRESSION] epoch {} ({}): {} -> {} tokens",
            c.epoch, c.trigger, c.tokens_before, c.tokens_after
        ),
        Event::CompressionFailed(f) => format!("[COMPRESSION] failed: {}", f.reason),
        Event::HarnessNote(n) => format!("[HARNESS] {}", n.text),
        Event::HarnessActivated(a) => {
            format!(
                "[HARNESS] activated {} (from {}, {})",
                a.to, a.from, a.source
            )
        }
        Event::Error(e) => format!("[ERROR] {}: {}", e.code, e.message),
        _ => return None,
    };
    Some(block)
}

fn action(call: &ToolCall, max: usize) -> String {
    let args = match &call.arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    };
    format!("[ACTION] {}({})", call.tool, clip(&args, max))
}

fn observation(result: &ToolResult, max: usize) -> String {
    if result.ok {
        let output = result.output.as_deref().unwrap_or_default();
        return format!("[OBSERVATION]\n{}", clip(output, max));
    }
    let error = result
        .error
        .as_ref()
        .map(|e| format!("{}: {}", e.kind, e.message))
        .unwrap_or_default();
    format!("[OBSERVATION ERROR]\n{}", clip(&error, max))
}

/// `text` cut to `max` characters, followed by
/// `…(+N chars — render truncation)` when anything was cut.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…(+{} chars — render truncation)", total - max)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RenderOptions, clip, render};
    use crate::trajectory::Line;

    fn line(seq: u64, kind: &str, payload: serde_json::Value) -> Line {
        Line {
            v: "1.0".to_owned(),
            ts: "2026-01-01T00:00:00.000000Z".to_owned(),
            seq,
            session_id: "ses_A".to_owned(),
            root_session_id: "ses_A".to_owned(),
            turn_id: None,
            span_id: format!("{seq:016x}"),
            parent_span_id: None,
            kind: kind.to_owned(),
            payload,
        }
    }

    #[test]
    fn an_oversized_observation_is_clipped_to_50000_characters() {
        let output = "x".repeat(80_000);
        let lines = [line(
            1,
            "tool.result",
            json!({"call_id": "c", "tool": "t", "ok": true, "duration_ms": 1, "output": output}),
        )];
        let text = render(&lines, &RenderOptions::default()).unwrap();
        let expected = format!(
            "[OBSERVATION]\n{}…(+30000 chars — render truncation)\n",
            "x".repeat(50_000)
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn the_observation_clip_never_goes_below_10000() {
        let output = "y".repeat(12_000);
        let lines = [line(
            1,
            "tool.result",
            json!({"call_id": "c", "tool": "t", "ok": true, "duration_ms": 1, "output": output}),
        )];
        let options = RenderOptions {
            obs_trunc: 10,
            ..RenderOptions::default()
        };
        let text = render(&lines, &options).unwrap();
        assert!(
            text.contains("…(+2000 chars — render truncation)"),
            "{text}"
        );
    }

    #[test]
    fn arguments_and_the_whole_output_are_clipped() {
        let lines = [
            line(1, "user.message", json!({"text": "hello"})),
            line(
                2,
                "tool.call",
                json!({"call_id": "c", "tool": "terminal", "arguments": {"command": "ls -la"}}),
            ),
        ];
        let options = RenderOptions {
            args_trunc: 5,
            ..RenderOptions::default()
        };
        assert_eq!(
            render(&lines, &options).unwrap(),
            "[USER]\nhello\n\n[ACTION] terminal({\"com…(+15 chars — render truncation))\n"
        );
        let options = RenderOptions {
            max_chars: Some(6),
            ..RenderOptions::default()
        };
        assert_eq!(
            render(&lines, &options).unwrap(),
            "[USER]…(+48 chars — render truncation)"
        );
    }

    #[test]
    fn clip_counts_characters_not_bytes() {
        assert_eq!(clip("ééé", 3), "ééé");
        assert_eq!(clip("éééé", 3), "ééé…(+1 chars — render truncation)");
    }
}
