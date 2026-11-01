//! A mock response as a stream: one chunk per piece of content (words of
//! text and reasoning, each tool call's start, arguments and end), each
//! after the rule's `latency_ms`, then the usage and the stop, or the
//! injected stream fault. Usage is ceil(UTF-8 bytes / 4) per message and
//! the cost is 0.

use std::collections::VecDeque;
use std::time::Duration;

use super::script::{Response, StreamFault};
use crate::classify::ProviderError;
use crate::event::{Normalizer, UsageReport};
use crate::request::{Part, Request};
use crate::transport::Limits;
use crate::wire;

/// One streamed piece.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    Reasoning(String),
    Text(String),
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolArguments {
        index: usize,
        text: String,
    },
    ToolEnd {
        index: usize,
    },
}

impl Chunk {
    fn apply(self, out: &mut Normalizer) {
        match self {
            Self::Reasoning(text) => out.reasoning(&text),
            Self::Text(text) => out.text(&text),
            Self::ToolStart { index, id, name } => out.tool_start(index, &id, &name),
            Self::ToolArguments { index, text } => out.tool_arguments(index, &text),
            Self::ToolEnd { index } => out.tool_end(index),
        }
    }
}

/// How the stream ends once its chunks are out.
#[derive(Debug)]
enum End {
    Finish(UsageReport),
    Fail(ProviderError),
}

/// A scripted response being streamed.
#[derive(Debug)]
pub(crate) struct Feed {
    chunks: VecDeque<Chunk>,
    end: Option<End>,
    latency: Duration,
}

impl Feed {
    /// The stream of `response` to `request` at model call `step`.
    pub(crate) fn new(
        response: &Response,
        request: &Request,
        step: usize,
        latency: Duration,
    ) -> Self {
        let mut chunks = VecDeque::new();
        if let Some(reasoning) = &response.reasoning {
            chunks.extend(words(reasoning).map(Chunk::Reasoning));
        }
        if let Some(text) = &response.text {
            chunks.extend(words(text).map(Chunk::Text));
        }
        for (index, call) in response.tool_calls.iter().enumerate() {
            let id = call
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{step}_{index}"));
            chunks.push_back(Chunk::ToolStart {
                index,
                id,
                name: call.name.clone(),
            });
            chunks.push_back(Chunk::ToolArguments {
                index,
                text: call.arguments.clone(),
            });
            chunks.push_back(Chunk::ToolEnd { index });
        }
        let end = match response.broken {
            None => End::Finish(usage(request, response)),
            Some(StreamFault::Truncated) => End::Fail(wire::truncated()),
            Some(StreamFault::Malformed) => {
                End::Fail(wire::malformed("mock: expected value at line 1 column 1"))
            }
        };
        Self {
            chunks,
            end: Some(end),
            latency,
        }
    }

    /// Streams the next chunk into `out`, or ends the stream.
    pub(crate) async fn step(
        &mut self,
        out: &mut Normalizer,
        limits: Limits,
    ) -> Result<(), ProviderError> {
        if !self.latency.is_zero() {
            limits.within(tokio::time::sleep(self.latency)).await?;
        }
        if let Some(chunk) = self.chunks.pop_front() {
            chunk.apply(out);
            return Ok(());
        }
        match self.end.take() {
            Some(End::Finish(report)) => {
                out.usage(report);
                out.finish(None);
                Ok(())
            }
            Some(End::Fail(error)) => Err(error),
            None => Err(wire::truncated()),
        }
    }
}

/// `text` in word-sized pieces (each keeps its trailing space).
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split_inclusive(' ').map(str::to_owned)
}

/// ceil(`bytes` / 4).
fn tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

fn part_bytes(part: &Part) -> usize {
    match part {
        Part::Text { text } => text.len(),
        Part::Image { data, .. } => data.len(),
        Part::ToolCall {
            name, arguments, ..
        } => name.len() + arguments.len(),
        Part::ToolResult { content, .. } => content.len(),
    }
}

/// Input: every system block and message counted on its own; output: the
/// response message (reasoning included); cost 0.
fn usage(request: &Request, response: &Response) -> UsageReport {
    let system: u64 = request.system.iter().map(|b| tokens(b.text.len())).sum();
    let messages: u64 = request
        .messages
        .iter()
        .map(|m| tokens(m.parts.iter().map(part_bytes).sum()))
        .sum();
    let reasoning = response.reasoning.as_deref().map_or(0, str::len);
    let calls: usize = response
        .tool_calls
        .iter()
        .map(|c| c.name.len() + c.arguments.len())
        .sum();
    let output = reasoning + response.text.as_deref().map_or(0, str::len) + calls;
    UsageReport {
        input: Some(system + messages),
        output: Some(tokens(output)),
        cache_read: Some(0),
        cache_write: Some(0),
        reasoning: Some(tokens(reasoning)),
        cost_usd: Some(0.0),
    }
}
