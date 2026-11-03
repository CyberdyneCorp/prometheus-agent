//! `pre_llm_call` context (spec: plugin-system, `pre_llm_call` context
//! preserves the prompt cache).
//!
//! The text callbacks return is appended by the turn loop to the current
//! turn's user message on the wire only: never to the system prompt and
//! never to stored history, so the cached prefix is untouched. One
//! callback's context above `plugins.hook_context_max_chars` is written to
//! `<profile_home>/hook_outputs/<session_id>/` and replaced by its first
//! and last [`SPILL_EDGE_CHARS`] characters and the file's path.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{Hooks, Payload};
use crate::host::CallInfo;
use crate::host::hook_world::Reply;

/// Characters kept from each end of a spilled context.
pub const SPILL_EDGE_CHARS: usize = 500;

/// `<profile_home>/hook_outputs/<session_id>/`.
#[must_use]
pub fn spill_dir(profile_dir: &Path, session_id: &str) -> PathBuf {
    profile_dir.join("hook_outputs").join(session_id)
}

impl Hooks {
    /// Fires `pre_llm_call` for one turn and returns the context to append
    /// to its user message (`None` when no callback added any).
    pub async fn pre_llm_call(
        &self,
        session_id: &str,
        user_message: &str,
        call: &CallInfo,
    ) -> Option<String> {
        if !self.handles("pre_llm_call") {
            return None;
        }
        let mut fields = Map::new();
        fields.insert("session_id".to_owned(), session_id.into());
        fields.insert("user_message".to_owned(), Value::from(user_message));
        let answers = self.fire("pre_llm_call", &Payload::new(fields), call).await;
        let mut parts = Vec::new();
        for (n, answer) in answers.iter().enumerate() {
            let Reply::Context(text) = &answer.reply else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            parts.push(self.bounded(session_id, &answer.plugin, n, text));
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn bounded(&self, session_id: &str, plugin: &str, n: usize, text: &str) -> String {
        if text.chars().count() <= self.settings.context_max_chars {
            return text.to_owned();
        }
        let dir = spill_dir(&self.settings.profile_dir, session_id);
        let path = dir.join(format!(
            "pre_llm_call-{plugin}-{}-{n}.txt",
            jiff::Timestamp::now().as_millisecond()
        ));
        let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, text));
        let head: String = text.chars().take(SPILL_EDGE_CHARS).collect();
        let tail: String = {
            let chars: Vec<char> = text.chars().collect();
            chars[chars.len().saturating_sub(SPILL_EDGE_CHARS)..]
                .iter()
                .collect()
        };
        match written {
            Ok(()) => format!(
                "{head}\n[... hook context truncated; full text in {} ...]\n{tail}",
                path.display()
            ),
            Err(err) => {
                tracing::warn!(
                    plugin,
                    "cannot spill hook context to {}: {err}",
                    path.display()
                );
                format!("{head}\n[... hook context truncated ...]\n{tail}")
            }
        }
    }
}
