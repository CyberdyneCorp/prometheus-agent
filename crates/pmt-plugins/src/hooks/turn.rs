//! The hooks of a turn's reply (spec: plugin-system, Transform hooks
//! rewrite results and replies; Streaming and lifecycle observer hooks;
//! `pre_verify` continuation hook): `transform_llm_output`, `pre_verify`
//! and the observers, which run on a worker of their own in firing order
//! so they never hold up the turn and cannot change what it delivers.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use super::{Hooks, Payload};
use crate::host::CallInfo;
use crate::host::hook_world::Reply;

/// What `transform_llm_output` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// No callback rewrote the reply.
    Unchanged,
    /// The first non-empty rewrite.
    Replaced(String),
    /// A callback that holds the stream failed or timed out without a
    /// rewrite: the reply must not be delivered.
    Withheld,
}

/// Observer firings queued per session before new ones are dropped.
const OBSERVER_QUEUE: usize = 1024;

impl Hooks {
    /// Runs `transform_llm_output`: the first non-empty answer wins. With
    /// `held`, a holding plugin that gave no answer (it failed or timed
    /// out) withholds the reply.
    pub async fn transform_llm_output(
        &self,
        fields: Map<String, Value>,
        call: &CallInfo,
        held: bool,
    ) -> Rewrite {
        const HOOK: &str = "transform_llm_output";
        if !self.handles(HOOK) {
            return Rewrite::Unchanged;
        }
        let answers = self.fire(HOOK, &Payload::new(fields), call).await;
        let rewrite = answers.iter().find_map(|a| match &a.reply {
            Reply::Context(text) if !text.is_empty() => Some(text.clone()),
            _ => None,
        });
        if let Some(text) = rewrite {
            return Rewrite::Replaced(text);
        }
        let silent_holder = self.callbacks.get(HOOK).is_some_and(|callbacks| {
            callbacks.iter().any(|c| {
                self.held.contains(&c.plugin) && !answers.iter().any(|a| a.plugin == c.plugin)
            })
        });
        if held && silent_holder {
            Rewrite::Withheld
        } else {
            Rewrite::Unchanged
        }
    }

    /// Runs `pre_verify`: the first `{action = "continue", message}` (or
    /// `{decision = "block", reason}`) answer's message.
    pub async fn pre_verify(&self, fields: Map<String, Value>, call: &CallInfo) -> Option<String> {
        const HOOK: &str = "pre_verify";
        if !self.handles(HOOK) {
            return None;
        }
        let answers = self.fire(HOOK, &Payload::new(fields), call).await;
        answers.iter().find_map(|answer| match &answer.reply {
            Reply::Modify(text) => continuation(text),
            _ => None,
        })
    }
}

#[derive(Deserialize)]
struct Continuation {
    action: Option<String>,
    message: Option<String>,
    decision: Option<String>,
    reason: Option<String>,
}

/// The message of a continuation answer.
#[must_use]
pub fn continuation(text: &str) -> Option<String> {
    let answer: Continuation = serde_json::from_str(text).ok()?;
    match (answer.action.as_deref(), answer.decision.as_deref()) {
        (Some("continue"), _) => answer.message,
        (_, Some("block")) => answer.reason,
        _ => None,
    }
}

/// One session's observer queue: firings run one after another on a
/// worker task, off the turn's path; their answers are discarded.
#[derive(Debug, Clone)]
pub struct Observers {
    sender: Option<mpsc::Sender<(&'static str, Map<String, Value>)>>,
}

impl Observers {
    /// A queue firing on whatever hooks `source` holds at each firing;
    /// without a tokio runtime nothing is fired.
    #[must_use]
    pub fn start(source: Arc<dyn super::HookSource>, call: CallInfo) -> Self {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Self { sender: None };
        };
        let (sender, mut receiver) =
            mpsc::channel::<(&'static str, Map<String, Value>)>(OBSERVER_QUEUE);
        pmt_core::spawn::spawn_on(&handle, async move {
            while let Some((hook, fields)) = receiver.recv().await {
                let hooks = source.hooks();
                if hooks.handles(hook) {
                    hooks.fire(hook, &Payload::new(fields), &call).await;
                }
            }
        });
        Self {
            sender: Some(sender),
        }
    }

    /// Queues `hook`; a full queue drops it with a warning.
    pub fn fire(&self, hook: &'static str, fields: Map<String, Value>) {
        let Some(sender) = &self.sender else {
            return;
        };
        if sender.try_send((hook, fields)).is_err() {
            tracing::warn!(hook, "observer hook dropped: its queue is full");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::continuation;

    #[test]
    fn both_continuation_shapes_are_read() {
        assert_eq!(
            continuation(r#"{"action":"continue","message":"run the tests"}"#).as_deref(),
            Some("run the tests")
        );
        assert_eq!(
            continuation(r#"{"decision":"block","reason":"lint first"}"#).as_deref(),
            Some("lint first")
        );
        assert_eq!(continuation(r#"{"action":"done"}"#), None);
    }
}
