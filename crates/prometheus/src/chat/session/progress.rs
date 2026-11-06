//! Tool activity for the line interface (spec: cli, Tool progress display
//! and focus view): the session's trajectory events, teed to a channel the
//! interface renders. Display only: nothing here reaches the prompt or the
//! tool set. Session notices (a provider fallback, a borrowed login's
//! first use) reach every surface: the line interface shows them, a
//! one-shot run writes them to stderr (spec: model-providers, In-turn
//! fallback activation; Borrowed CLI logins).

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
    /// A notice about the session itself.
    Notice(String),
}

/// Where session notices go: the line interface's channel, else stderr.
#[derive(Clone)]
pub struct Notices {
    activity: Option<mpsc::UnboundedSender<Progress>>,
}

impl Notices {
    /// Shows `text` once.
    pub fn show(&self, text: String) {
        match &self.activity {
            Some(activity) => {
                let _ = activity.send(Progress::Notice(text));
            }
            None => eprintln!("{text}"),
        }
    }
}

/// The notice of a provider fallback.
#[must_use]
pub fn fallback_notice(fallback: &pmt_obs::trajectory::payload::ProviderFallback) -> String {
    let mut notice = format!(
        "switched from {} to {} ({})",
        fallback.from, fallback.to, fallback.reason
    );
    if let (true, Some(from), Some(to)) = (
        fallback.billing_change,
        &fallback.from_billing,
        &fallback.to_billing,
    ) {
        use pmt_providers::fallback::billing_label;
        notice = format!(
            "{notice}; now billing {} instead of {}",
            billing_label(to),
            billing_label(from)
        );
    }
    notice
}

/// Forwards every event to `inner`, tool activity to the line interface
/// and notices to [`Notices`].
pub struct Tee {
    inner: Arc<dyn EventSink>,
    notices: Notices,
    tree: std::sync::Mutex<pmt_core::subagent_tree::TreeLines>,
}

impl Tee {
    /// The sink and, for the line interface (`interactive`), the receiving
    /// end of its activity; without it notices go to stderr.
    #[must_use]
    pub fn new(
        inner: Arc<dyn EventSink>,
        interactive: bool,
    ) -> (Self, Option<mpsc::UnboundedReceiver<Progress>>) {
        let (activity, rx) = if interactive {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let notices = Notices { activity };
        let tree = std::sync::Mutex::default();
        (
            Self {
                inner,
                notices,
                tree,
            },
            rx,
        )
    }

    /// The session's notices.
    #[must_use]
    pub fn notices(&self) -> Notices {
        self.notices.clone()
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
            Event::SubagentSpawned(_) | Event::SubagentProgress(_) | Event::SubagentFinished(_) => {
                let payload = emit.event.payload();
                let line = self
                    .tree
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .line(emit.event.kind(), &payload);
                if let Some(line) = line {
                    self.notices.show(line);
                }
                None
            }
            Event::ProviderFallback(fallback) => {
                self.notices.show(fallback_notice(fallback));
                None
            }
            _ => None,
        };
        if let (Some(progress), Some(activity)) = (progress, &self.notices.activity) {
            let _ = activity.send(progress);
        }
        self.inner.emit(emit)
    }
}

/// Sends every event to two sinks, in order (a daemon session's trajectory
/// and its attached clients).
pub struct Both(pub Arc<dyn EventSink>, pub Arc<dyn EventSink>);

impl EventSink for Both {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        Box::pin(async move {
            let first = self.0.emit(emit.clone()).await;
            self.1.emit(emit).await?;
            first
        })
    }
}

#[cfg(test)]
mod tests {
    use pmt_obs::trajectory::payload::ProviderFallback;

    use super::fallback_notice;

    #[test]
    fn a_billing_change_names_both_sources() {
        let fallback = ProviderFallback {
            from: "openai-codex/gpt-5".to_owned(),
            to: "openrouter/m".to_owned(),
            reason: "auth.credentials_benched".to_owned(),
            billing_change: true,
            from_billing: Some("subscription:openai-codex/personal".to_owned()),
            to_billing: Some("api_key:openrouter/default".to_owned()),
        };
        assert_eq!(
            fallback_notice(&fallback),
            "switched from openai-codex/gpt-5 to openrouter/m (auth.credentials_benched); now billing API key openrouter/default instead of subscription openai-codex/personal"
        );
        let same = ProviderFallback {
            billing_change: false,
            ..fallback
        };
        assert!(!fallback_notice(&same).contains("billing"));
    }
}
