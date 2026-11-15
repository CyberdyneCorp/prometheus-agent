//! What makes a session a harness-evolution rollout (spec:
//! harness-evolution, Isolated rollouts; Side-effect dry-run in rollouts):
//! the candidate harness instead of the profile's active version, the
//! task's toolsets without interactive tools or the memory tool, and a
//! [`Recorder`] in front of every `effect = external` tool, which stores
//! the call and answers without running it. The terminal and process
//! tools stay live: they run inside the rollout's own sandbox and
//! workspace, under its egress limit, so their effects never leave it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pmt_agent::turn::Declared;
use pmt_agent::turn::model::RequestedCall;
use pmt_core::error::Error;
use pmt_tools::{Selection, ToolRegistry, ToolResult};
use serde_json::Value;

use crate::replay::{ToolAnswer, ToolReplay};

/// What a recorded call returns.
pub const RECORDED: &str = "recorded, not executed during harness evolution";

/// External tools whose effects the rollout sandbox contains.
const CONFINED: &[&str] = &["terminal", "process"];

/// The memory write tool.
const MEMORY: &str = "memory";

/// A rollout session's settings.
#[derive(Debug, Clone)]
pub struct RolloutRun {
    /// The candidate harness tree.
    pub harness: PathBuf,
    /// The task's toolsets (`None` or empty: the CLI surface's).
    pub toolsets: Option<Vec<String>>,
    /// Records the external calls.
    pub recorder: Arc<Recorder>,
}

impl RolloutRun {
    /// The rollout's tools: the task's toolsets (or `base`), without
    /// interactive tools and without the memory tool (rollouts never
    /// write memory).
    ///
    /// # Errors
    ///
    /// An unknown toolset.
    pub fn selection(
        &self,
        registry: &ToolRegistry,
        disabled: &[String],
        base: Selection,
    ) -> Result<Selection, Error> {
        let selection = match self.toolsets.as_deref() {
            Some(toolsets) if !toolsets.is_empty() => {
                Selection::resolve(registry, toolsets, disabled)?
            }
            _ => base,
        };
        let mut selection = selection.without_interactive(registry);
        if selection.tools.iter().any(|t| t == MEMORY) {
            selection.tools.retain(|t| t != MEMORY);
            selection.disabled.insert(MEMORY.to_owned());
        }
        Ok(selection)
    }
}

/// Answers `effect = external` calls with [`RECORDED`] and keeps them.
#[derive(Debug, Default)]
pub struct Recorder {
    calls: Mutex<Vec<(String, Value)>>,
}

impl Recorder {
    /// The recorded calls, in order: tool and arguments.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ToolReplay for Recorder {
    fn answer(&self, call: &RequestedCall, declared: Option<&Declared>) -> ToolAnswer {
        let external = declared.is_some_and(|d| d.effect == "external");
        if !external || CONFINED.contains(&call.name.as_str()) {
            return ToolAnswer::Live;
        }
        let arguments =
            serde_json::from_str(&call.arguments).unwrap_or(Value::String(call.arguments.clone()));
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((call.name.clone(), arguments));
        ToolAnswer::Serve(ToolResult::ok(RECORDED))
    }

    fn recorded_tools(&self) -> Vec<pmt_providers::request::Tool> {
        Vec::new()
    }
}

impl super::Factory {
    /// The harness tree session `id` runs with: a rollout's candidate,
    /// else the tree of the version recorded on its row.
    pub(super) fn harness_dir(&self, id: pmt_core::id::SessionId) -> Option<PathBuf> {
        if let Some(run) = &self.options.rollout {
            return Some(run.harness.clone());
        }
        crate::harness_host::session_version(&self.scope, &self.store, id)
            .map(|version| pmt_config::harness::HarnessDir::of(self.scope.dir()).tree_dir(&version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(effect: &str) -> Declared {
        Declared {
            effect: effect.to_owned(),
            idempotent: false,
        }
    }

    fn call(name: &str, arguments: &str) -> RequestedCall {
        RequestedCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Scenario: Message not sent (the recorder's half).
    #[test]
    fn external_calls_are_recorded_and_confined_ones_run() {
        let recorder = Recorder::default();
        let sent = recorder.answer(
            &call("send_message", r#"{"target":"telegram:42","message":"hi"}"#),
            Some(&declared("external")),
        );
        match sent {
            ToolAnswer::Serve(result) => assert_eq!(result.text_content(), RECORDED),
            ToolAnswer::Live => panic!("send_message ran"),
        }
        assert!(matches!(
            recorder.answer(&call("terminal", "{}"), Some(&declared("external"))),
            ToolAnswer::Live
        ));
        assert!(matches!(
            recorder.answer(&call("read_file", "{}"), Some(&declared("none"))),
            ToolAnswer::Live
        ));
        assert_eq!(
            recorder.calls(),
            [(
                "send_message".to_owned(),
                serde_json::json!({"target": "telegram:42", "message": "hi"})
            )]
        );
    }
}
