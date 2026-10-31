//! Tool activity for the line interface (spec: cli, Tool progress display
//! and focus view): the session's trajectory events, teed to a channel the
//! interface renders. Display only: nothing here reaches the prompt or the
//! tool set.

use std::sync::Arc;

use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::writer::Emit;
use serde_json::Value;
use tokio::sync::mpsc;

/// One piece of tool activity.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// A tool call starts.
    Call {
        /// The tool.
        tool: String,
        /// Its arguments.
        arguments: Value,
    },
    /// A tool call ended.
    Result {
        /// The tool.
        tool: String,
        /// It succeeded.
        ok: bool,
        /// Its output or error message.
        text: String,
    },
    /// Assistant text (interim when a tool call follows it).
    Assistant(String),
    /// Readable reasoning.
    Reasoning(String),
}

/// Forwards every event to `inner` and tool activity to the channel.
pub struct Tee {
    inner: Arc<dyn EventSink>,
    activity: mpsc::UnboundedSender<Progress>,
}

impl Tee {
    /// The sink and the receiving end of its activity.
    #[must_use]
    pub fn new(inner: Arc<dyn EventSink>) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        let (activity, rx) = mpsc::unbounded_channel();
        (Self { inner, activity }, rx)
    }
}

impl EventSink for Tee {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        let progress = match &emit.event {
            Event::ToolCall(call) => Some(Progress::Call {
                tool: call.tool.clone(),
                arguments: call.arguments.clone(),
            }),
            Event::ToolResult(result) => Some(Progress::Result {
                tool: result.tool.clone(),
                ok: result.ok,
                text: result
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .or_else(|| result.output.clone())
                    .unwrap_or_default(),
            }),
            Event::AssistantMessage(message) => Some(Progress::Assistant(message.text.clone())),
            Event::AssistantReasoning(reasoning) => reasoning.text.clone().map(Progress::Reasoning),
            _ => None,
        };
        if let Some(progress) = progress {
            let _ = self.activity.send(progress);
        }
        self.inner.emit(emit)
    }
}
