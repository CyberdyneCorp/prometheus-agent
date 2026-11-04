//! `transform_terminal_output` (spec: execution-environments, Foreground
//! result shaping): a session's foreground terminal output passes through
//! its profile's hooks (plugins, bridged plugins, shell hooks) before ANSI
//! stripping, redaction and truncation. The first text answer replaces
//! the output; no answer leaves it unchanged.

use std::sync::Arc;

use pmt_plugins::hooks::{HookSource as _, Payload};
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::Reply;
use pmt_tools::dispatch::SeamFuture;
use pmt_tools::terminal::shape::{OutputHook, TransformOutput};
use serde_json::{Map, Value};

use super::ProfilePlugins;

/// The hook name.
const HOOK: &str = "transform_terminal_output";

/// The terminal's hook over the profile's live hook set (so a reload is
/// seen by the next command).
#[must_use]
pub fn output_hook(plugins: ProfilePlugins, call: CallInfo) -> OutputHook {
    Arc::new(TerminalHook { plugins, call })
}

struct TerminalHook {
    plugins: ProfilePlugins,
    call: CallInfo,
}

impl TransformOutput for TerminalHook {
    fn transform(&self, output: String) -> SeamFuture<'_, String> {
        Box::pin(async move {
            let hooks = self.plugins.handle.hooks();
            if !hooks.handles(HOOK) {
                return output;
            }
            let mut fields = Map::new();
            fields.insert(
                "session_id".to_owned(),
                self.call
                    .session_id
                    .clone()
                    .map_or(Value::Null, Value::String),
            );
            fields.insert("output".to_owned(), Value::String(output.clone()));
            let answers = hooks.fire(HOOK, &Payload::new(fields), &self.call).await;
            answers
                .into_iter()
                .find_map(|a| match a.reply {
                    Reply::Context(text) => Some(text),
                    _ => None,
                })
                .unwrap_or(output)
        })
    }
}
