//! Tool loop guardrails (spec: agent-turn-loop, Tool loop guardrails).
//!
//! Per turn, three repetition counters: the same tool failing with the same
//! arguments and error, the same tool failing at all, and an idempotent
//! call returning the result it returned before. Reaching a `warn_after`
//! threshold appends a warning to that tool result (never the system
//! prompt); reaching `hard_stop_after` ends the turn with `guardrail_stop`
//! when hard stops apply (always when `hard_stop_enabled`, and on
//! non-interactive surfaces when `non_interactive_hard_stop_enabled`). The
//! per-turn call caps are always enforced: a call over a cap is refused
//! with an error result and never runs.

use std::collections::HashMap;

use pmt_config::schema::{GuardrailThresholds, ToolLoopGuardrails};
use pmt_tools::{Content, ToolResult};

use super::model::RequestedCall;

/// The tool a `max_web_searches` cap counts.
const WEB_SEARCH: &str = "web_search";
/// The tool a `max_subagents` cap counts.
const DELEGATE: &str = "delegate_task";

/// One turn's guardrail state.
#[derive(Debug)]
pub(super) struct LoopGuard {
    config: ToolLoopGuardrails,
    hard_stop: bool,
    exact: HashMap<(String, String, String), u32>,
    by_tool: HashMap<String, u32>,
    /// Per idempotent call: the last result digest and how many times in a
    /// row it came back.
    repeats: HashMap<(String, String), (String, u32)>,
    searches: u32,
    subagents: u32,
}

/// Which repetition a count is of.
#[derive(Debug, Clone, Copy)]
enum Repeat {
    ExactFailure,
    SameToolFailure,
    NoProgress,
}

impl Repeat {
    const fn threshold(self, t: &GuardrailThresholds) -> u32 {
        match self {
            Self::ExactFailure => t.exact_failure,
            Self::SameToolFailure => t.same_tool_failure,
            Self::NoProgress => t.idempotent_no_progress,
        }
    }

    fn describe(self, tool: &str, count: u32) -> String {
        match self {
            Self::ExactFailure => format!(
                "this exact {tool} call has failed {count} times with the same error; change the arguments or the approach instead of repeating it"
            ),
            Self::SameToolFailure => format!(
                "{tool} has failed {count} times in this turn; step back and try a different approach"
            ),
            Self::NoProgress => format!(
                "this {tool} call returned the same result {count} times; repeating it will not make progress"
            ),
        }
    }
}

impl LoopGuard {
    /// The guard of a turn on an `interactive` (CLI, TUI, desktop, ACP)
    /// or non-interactive surface.
    pub(super) fn new(config: &ToolLoopGuardrails, interactive: bool) -> Self {
        Self {
            hard_stop: config.hard_stop_enabled
                || (!interactive && config.non_interactive_hard_stop_enabled),
            config: config.clone(),
            exact: HashMap::new(),
            by_tool: HashMap::new(),
            repeats: HashMap::new(),
            searches: 0,
            subagents: 0,
        }
    }

    /// The refusal of a call over a per-turn cap (it then never runs).
    pub(super) fn refusal(&mut self, call: &RequestedCall) -> Option<ToolResult> {
        let caps = &self.config.loop_caps;
        let (count, cap, key) = match call.name.as_str() {
            WEB_SEARCH => (
                &mut self.searches,
                caps.max_web_searches,
                "max_web_searches",
            ),
            DELEGATE => (&mut self.subagents, caps.max_subagents, "max_subagents"),
            _ => return None,
        };
        *count += 1;
        (cap != 0 && *count > cap).then(|| {
            ToolResult::error(format!(
                "{} refused: this turn reached tool_loop_guardrails.loop_caps.{key} ({cap} calls). Work with the results you have.",
                call.name
            ))
        })
    }

    /// Counts `result` of `call`, appends a warning when a threshold is
    /// reached and returns why the turn must stop, if it must.
    pub(super) fn observe(
        &mut self,
        call: &RequestedCall,
        result: &mut ToolResult,
        idempotent: bool,
    ) -> Option<String> {
        let mut reached = Vec::new();
        if result.status.succeeded() {
            if idempotent {
                let digest = crate::sha256(result.text_content().as_bytes());
                let entry = self
                    .repeats
                    .entry((call.name.clone(), call.arguments.clone()))
                    .or_insert_with(|| (digest.clone(), 0));
                if entry.0 == digest {
                    entry.1 += 1;
                } else {
                    *entry = (digest, 1);
                }
                reached.push((Repeat::NoProgress, entry.1));
            }
        } else {
            let key = (
                call.name.clone(),
                call.arguments.clone(),
                result.text_content(),
            );
            let exact = self.exact.entry(key).or_default();
            *exact += 1;
            let by_tool = self.by_tool.entry(call.name.clone()).or_default();
            *by_tool += 1;
            reached.push((Repeat::ExactFailure, *exact));
            reached.push((Repeat::SameToolFailure, *by_tool));
        }
        self.verdict(&call.name, result, &reached)
    }

    fn verdict(
        &self,
        tool: &str,
        result: &mut ToolResult,
        reached: &[(Repeat, u32)],
    ) -> Option<String> {
        let stop = reached
            .iter()
            .find(|(repeat, count)| {
                self.hard_stop && *count >= repeat.threshold(&self.config.hard_stop_after)
            })
            .map(|(repeat, count)| {
                format!(
                    "Stopped by the tool loop guardrail: {}.",
                    repeat.describe(tool, *count)
                )
            });
        let warning = reached
            .iter()
            .find(|(repeat, count)| *count >= repeat.threshold(&self.config.warn_after));
        if let (true, Some((repeat, count))) = (self.config.warnings_enabled, warning) {
            result.content.push(Content::Text {
                text: format!("[prometheus] Warning: {}.", repeat.describe(tool, *count)),
            });
        }
        stop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> RequestedCall {
        RequestedCall {
            id: "c".into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn identical_failures_warn_from_the_second_and_stop_at_the_fifth_when_unattended() {
        let mut guard = LoopGuard::new(&ToolLoopGuardrails::default(), false);
        let c = call("terminal", r#"{"command":"make"}"#);
        for n in 1..=5 {
            let mut result = ToolResult::error("exit 2");
            let stop = guard.observe(&c, &mut result, false);
            assert_eq!(result.text_content().contains("Warning"), n >= 2, "{n}");
            assert_eq!(stop.is_some(), n == 5, "{n}");
        }
    }

    #[test]
    fn interactive_sessions_only_warn_by_default() {
        let mut guard = LoopGuard::new(&ToolLoopGuardrails::default(), true);
        let c = call("terminal", "{}");
        for _ in 0..10 {
            assert!(
                guard
                    .observe(&c, &mut ToolResult::error("x"), false)
                    .is_none()
            );
        }
    }

    #[test]
    fn idempotent_calls_returning_the_same_result_are_counted() {
        let mut guard = LoopGuard::new(&ToolLoopGuardrails::default(), true);
        let c = call("read_file", r#"{"path":"a"}"#);
        let mut first = ToolResult::ok("same");
        guard.observe(&c, &mut first, true);
        assert!(!first.text_content().contains("Warning"));
        let mut second = ToolResult::ok("same");
        guard.observe(&c, &mut second, true);
        assert!(
            second
                .text_content()
                .contains("returned the same result 2 times")
        );
        let mut changed = ToolResult::ok("new");
        guard.observe(&c, &mut changed, true);
        assert!(!changed.text_content().contains("Warning"));
    }

    #[test]
    fn caps_refuse_calls_beyond_the_limit_and_zero_is_unlimited() {
        let mut guard = LoopGuard::new(&ToolLoopGuardrails::default(), true);
        let search = call(WEB_SEARCH, "{}");
        for _ in 0..50 {
            assert!(guard.refusal(&search).is_none());
        }
        let refused = guard.refusal(&search).unwrap();
        assert!(
            refused
                .text_content()
                .contains("tool_loop_guardrails.loop_caps.max_web_searches")
        );
        let mut config = ToolLoopGuardrails::default();
        config.loop_caps.max_subagents = 0;
        let mut unlimited = LoopGuard::new(&config, true);
        for _ in 0..200 {
            assert!(unlimited.refusal(&call(DELEGATE, "{}")).is_none());
        }
    }
}
