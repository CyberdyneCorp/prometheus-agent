//! The normalised stream (spec: model-providers, Wire formats, selection
//! and normalised event stream): every wire format becomes one ordered
//! stream of [`StreamEvent`]s ending in exactly one `stop`.
//!
//! Wire decoders report what they see to a [`Normalizer`], which owns the
//! ordering rules: a tool call is started, extended and ended in order with
//! canonical JSON arguments, one `usage` (zeros when unreported) precedes
//! the `stop`, and nothing follows it.

use std::collections::BTreeMap;

use serde::Serialize;

/// Why the model stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The answer is complete.
    EndTurn,
    /// The model wants tool results.
    ToolUse,
    /// The output cap was hit.
    MaxTokens,
    /// The provider filtered the output.
    ContentFilter,
    /// The stream failed after it started.
    Error,
}

/// Token usage of one call. `input_tokens` excludes cached input;
/// `reasoning_tokens` are part of `output_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens (reasoning included).
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Cost the provider reported, in USD.
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// The ledger's token classes.
    #[must_use]
    pub fn tokens(&self) -> pmt_store::Tokens {
        pmt_store::Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_tokens,
            cache_write: self.cache_write_tokens,
            reasoning: self.reasoning_tokens,
        }
    }
}

/// One normalised stream event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Answer text.
    TextDelta {
        /// The text.
        text: String,
    },
    /// Reasoning text.
    ReasoningDelta {
        /// The text.
        text: String,
    },
    /// A tool call begins.
    ToolCallStart {
        /// Position among this response's tool calls.
        index: usize,
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
    },
    /// More argument text of a tool call.
    ToolCallDelta {
        /// Position.
        index: usize,
        /// Raw argument fragment.
        arguments: String,
    },
    /// A tool call is complete.
    ToolCallEnd {
        /// Position.
        index: usize,
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments as canonical JSON text (the raw text when not JSON).
        arguments: String,
    },
    /// Token usage (exactly one, before `stop`).
    Usage {
        /// The usage.
        usage: Usage,
    },
    /// The end of the response (exactly one, last).
    Stop {
        /// Why.
        stop_reason: StopReason,
    },
}

#[derive(Debug, Default)]
struct OpenCall {
    position: usize,
    id: String,
    name: String,
    arguments: String,
}

/// Token counts as a wire format reports them; `None` keeps what an
/// earlier report said (Anthropic splits input and output across events).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UsageReport {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// Turns decoder observations into the ordered [`StreamEvent`] stream.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    out: Vec<StreamEvent>,
    /// Open calls by the wire format's own index.
    open: BTreeMap<usize, OpenCall>,
    usage: Usage,
    /// Tool calls started so far (the next call's position).
    calls: usize,
    saw_tool_call: bool,
    finished: bool,
}

impl Normalizer {
    pub(crate) fn text(&mut self, text: &str) {
        if !text.is_empty() && !self.finished {
            self.out.push(StreamEvent::TextDelta {
                text: text.to_owned(),
            });
        }
    }

    pub(crate) fn reasoning(&mut self, text: &str) {
        if !text.is_empty() && !self.finished {
            self.out.push(StreamEvent::ReasoningDelta {
                text: text.to_owned(),
            });
        }
    }

    /// A tool call at wire index `index` starts (idempotent for repeated
    /// headers). Events carry the call's position among this response's
    /// tool calls, so every wire format numbers them the same way.
    pub(crate) fn tool_start(&mut self, index: usize, id: &str, name: &str) {
        if self.finished || self.open.contains_key(&index) {
            return;
        }
        self.saw_tool_call = true;
        let position = self.calls;
        self.calls += 1;
        self.open.insert(
            index,
            OpenCall {
                position,
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: String::new(),
            },
        );
        self.out.push(StreamEvent::ToolCallStart {
            index: position,
            id: id.to_owned(),
            name: name.to_owned(),
        });
    }

    pub(crate) fn tool_arguments(&mut self, index: usize, fragment: &str) {
        if fragment.is_empty() || self.finished {
            return;
        }
        if let Some(call) = self.open.get_mut(&index) {
            call.arguments.push_str(fragment);
            self.out.push(StreamEvent::ToolCallDelta {
                index: call.position,
                arguments: fragment.to_owned(),
            });
        }
    }

    pub(crate) fn tool_end(&mut self, index: usize) {
        if let Some(call) = self.open.remove(&index) {
            self.out.push(StreamEvent::ToolCallEnd {
                index: call.position,
                id: call.id,
                name: call.name,
                arguments: canonical_json(&call.arguments),
            });
        }
    }

    pub(crate) fn usage(&mut self, report: UsageReport) {
        let u = &mut self.usage;
        let keep = |slot: &mut u64, value: Option<u64>| {
            if let Some(v) = value {
                *slot = v;
            }
        };
        keep(&mut u.input_tokens, report.input);
        keep(&mut u.output_tokens, report.output);
        keep(&mut u.cache_read_tokens, report.cache_read);
        keep(&mut u.cache_write_tokens, report.cache_write);
        keep(&mut u.reasoning_tokens, report.reasoning);
        if report.cost_usd.is_some() {
            u.cost_usd = report.cost_usd;
        }
    }

    /// Ends the stream: closes open tool calls, emits usage and the one
    /// `stop`. `reason` `None` means "ended normally": `tool_use` when a
    /// tool call was made, else `end_turn`.
    pub(crate) fn finish(&mut self, reason: Option<StopReason>) {
        if self.finished {
            return;
        }
        let indexes: Vec<usize> = self.open.keys().copied().collect();
        for index in indexes {
            self.tool_end(index);
        }
        let stop_reason = match reason {
            Some(StopReason::EndTurn) | None if self.saw_tool_call => StopReason::ToolUse,
            Some(reason) => reason,
            None => StopReason::EndTurn,
        };
        self.out.push(StreamEvent::Usage { usage: self.usage });
        self.out.push(StreamEvent::Stop { stop_reason });
        self.finished = true;
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Events produced since the last drain.
    pub(crate) fn drain(&mut self) -> std::vec::Drain<'_, StreamEvent> {
        self.out.drain(..)
    }
}

/// `text` re-serialised as compact JSON, so the same arguments compare
/// equal whatever spacing the provider streamed; non-JSON is kept as is.
fn canonical_json(text: &str) -> String {
    if text.trim().is_empty() {
        return "{}".to_owned();
    }
    serde_json::from_str::<serde_json::Value>(text)
        .map_or_else(|_| text.to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_usage_and_stop_close_the_stream() {
        let mut n = Normalizer::default();
        n.text("a");
        n.tool_start(3, "c1", "read_file");
        n.tool_arguments(3, "{\"path\": ");
        n.tool_arguments(3, "\"x\"}");
        n.finish(None);
        n.finish(Some(StopReason::EndTurn));
        n.text("late");
        let events: Vec<_> = n.drain().collect();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                StreamEvent::TextDelta { .. } => "text",
                StreamEvent::ToolCallStart { .. } => "start",
                StreamEvent::ToolCallDelta { .. } => "delta",
                StreamEvent::ToolCallEnd { .. } => "end",
                StreamEvent::Usage { .. } => "usage",
                StreamEvent::Stop { .. } => "stop",
                StreamEvent::ReasoningDelta { .. } => "reasoning",
            })
            .collect();
        assert_eq!(
            kinds,
            ["text", "start", "delta", "delta", "end", "usage", "stop"]
        );
        assert_eq!(
            events[4],
            StreamEvent::ToolCallEnd {
                index: 0,
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"x\"}".into()
            }
        );
        assert_eq!(
            events[6],
            StreamEvent::Stop {
                stop_reason: StopReason::ToolUse
            }
        );
    }
}
