//! The tool result budget (spec: tool-system, Result budget and
//! spillover).
//!
//! A result whose text is longer than its threshold is written to
//! `<profile>/cache/spillover/<session_id>/<call_id>.txt` (0600, redacted)
//! and replaced by a `<persisted-output>` block with the size, the path,
//! recovery guidance and a 1,500-character preview. Thresholds resolve as:
//! `read_file` is never spilled (it paginates itself), then
//! `tools.result_budget.overrides.<tool>`, then `mcp_chars` for MCP tools,
//! then the tool's declared cap, then `default_chars`. With a known
//! context window, results are also held to 15% of the window (30% per
//! turn) at 4 characters per token, floored at 8,000 (16,000) characters;
//! otherwise one response's results may total 200,000 characters. Over
//! the per-turn budget the largest results are spilled first.

use std::path::{Path, PathBuf};

use pmt_config::schema::ResultBudget as BudgetConfig;
use pmt_core::private_fs;
use pmt_core::secret::Redactor;
use serde_json::Value;

use crate::descriptor::{Owner, RegisteredTool};
use crate::result::ToolResult;
use crate::spillover;

/// Characters of a spilled result shown inline.
pub const PREVIEW_CHARS: usize = 1_500;
/// Per-turn budget without a known context window.
pub const DEFAULT_TURN_CHARS: usize = 200_000;
/// Floors of the context-derived budgets.
pub const MIN_RESULT_CHARS: usize = 8_000;
/// Floor of the context-derived per-turn budget.
pub const MIN_TURN_CHARS: usize = 16_000;
/// Metadata key holding the spill file path.
pub const SPILL_KEY: &str = "full_output_path";
/// The tool that pages through files itself and is never spilled.
const SELF_PAGING: &str = "read_file";
const CHARS_PER_TOKEN: u64 = 4;

/// The budget of one session.
#[derive(Debug, Clone)]
pub struct ResultBudget {
    config: BudgetConfig,
    context_window: Option<u64>,
    dir: PathBuf,
    redactor: Redactor,
}

/// One result of a turn, for [`ResultBudget::apply_turn`].
#[derive(Debug)]
pub struct TurnResult<'a> {
    /// The model's call id.
    pub call_id: &'a str,
    /// The tool, when the name resolved.
    pub tool: Option<&'a RegisteredTool>,
    /// The result, changed in place.
    pub result: &'a mut ToolResult,
}

impl ResultBudget {
    /// The budget of a session spilling into `dir`
    /// ([`crate::spillover::session_dir`]); `context_window` is the main
    /// model's window in tokens, when known. Spilled text is redacted with
    /// `redactor`.
    #[must_use]
    pub fn new(
        config: BudgetConfig,
        context_window: Option<u64>,
        dir: PathBuf,
        redactor: Redactor,
    ) -> Self {
        Self {
            config,
            context_window,
            dir,
            redactor,
        }
    }

    /// The budget of session `session_id` of the profile at `profile_dir`
    /// with its `[tools.result_budget]` and `[sessions]` config. Starting a
    /// session first prunes the profile's expired spillover files.
    #[must_use]
    pub fn for_session(
        profile_dir: &Path,
        session_id: &str,
        config: &pmt_config::schema::Config,
        context_window: Option<u64>,
        redactor: Redactor,
    ) -> Self {
        spillover::prune_profile(profile_dir, &config.sessions);
        Self::new(
            config.tools.result_budget.clone(),
            context_window,
            spillover::session_dir(profile_dir, session_id),
            redactor,
        )
    }

    /// The inline limit of `tool`'s results; `None` = never spilled.
    #[must_use]
    pub fn threshold(&self, tool: Option<&RegisteredTool>) -> Option<usize> {
        let configured = match tool {
            Some(t) if t.owner == Owner::Builtin && t.name() == SELF_PAGING => return None,
            Some(t) => self.configured(t),
            None => to_usize(self.config.default_chars),
        };
        Some(match self.window_chars(15, MIN_RESULT_CHARS) {
            Some(window) => configured.min(window),
            None => configured,
        })
    }

    fn configured(&self, tool: &RegisteredTool) -> usize {
        if let Some(&chars) = self.config.overrides.get(tool.name()) {
            return to_usize(chars);
        }
        if matches!(tool.owner, Owner::Mcp(_)) {
            return to_usize(self.config.mcp_chars);
        }
        tool.descriptor
            .result_cap
            .unwrap_or_else(|| to_usize(self.config.default_chars))
    }

    /// The characters all results of one response may total.
    #[must_use]
    pub fn per_turn(&self) -> usize {
        self.window_chars(30, MIN_TURN_CHARS)
            .unwrap_or(DEFAULT_TURN_CHARS)
    }

    /// `percent` of the context window in characters, floored at `floor`.
    fn window_chars(&self, percent: u64, floor: usize) -> Option<usize> {
        let window = self.context_window?;
        let chars = window
            .saturating_mul(CHARS_PER_TOKEN)
            .saturating_mul(percent)
            / 100;
        Some(to_usize(chars).max(floor))
    }

    /// Spills `result` when it exceeds its tool's threshold.
    pub fn apply(&self, call_id: &str, tool: Option<&RegisteredTool>, result: &mut ToolResult) {
        if let Some(limit) = self.threshold(tool)
            && result.text_chars() > limit
        {
            self.spill(call_id, limit, result);
        }
    }

    /// Spills the largest spillable results of one response until they fit
    /// the per-turn budget.
    pub fn apply_turn(&self, results: &mut [TurnResult<'_>]) {
        let budget = self.per_turn();
        let mut total: usize = results.iter().map(|r| r.result.text_chars()).sum();
        while total > budget {
            let largest = results
                .iter_mut()
                .filter(|r| self.threshold(r.tool).is_some() && !spilled(r.result))
                .max_by_key(|r| r.result.text_chars());
            let Some(entry) = largest else {
                break;
            };
            let before = entry.result.text_chars();
            let limit = self.threshold(entry.tool).unwrap_or(0).min(budget);
            self.spill(entry.call_id, limit, entry.result);
            total = total - before + entry.result.text_chars();
        }
    }

    /// Writes the text to the spill file and replaces it with the
    /// `<persisted-output>` block, or truncates it inline to `limit` when
    /// the file cannot be written.
    fn spill(&self, call_id: &str, limit: usize, result: &mut ToolResult) {
        let text = result.text_content();
        let chars = text.chars().count();
        match self.write(call_id, &text) {
            Ok(path) => {
                result.replace_text(persisted_block(chars, &path, &text));
                result
                    .metadata
                    .insert(SPILL_KEY.into(), Value::String(path.display().to_string()));
            }
            Err(err) => {
                tracing::warn!("tool result spill failed: {err}; truncating inline");
                let kept: String = text.chars().take(limit).collect();
                result.replace_text(format!(
                    "{kept}\n[Truncated: tool response was {chars} chars; full output could not be saved]"
                ));
                result.metadata.insert(SPILL_KEY.into(), Value::Null);
            }
        }
    }

    fn write(&self, call_id: &str, text: &str) -> std::io::Result<PathBuf> {
        private_fs::create_dir(&self.dir)?;
        let path = self.dir.join(format!("{}.txt", file_safe(call_id)));
        private_fs::write_atomic(&path, self.redactor.redact(text).as_bytes())?;
        Ok(path)
    }
}

/// Whether a spill (or failed spill) already replaced this result's text.
fn spilled(result: &ToolResult) -> bool {
    result.metadata.contains_key(SPILL_KEY)
}

fn persisted_block(chars: usize, path: &Path, text: &str) -> String {
    let preview: String = text.chars().take(PREVIEW_CHARS).collect();
    let more = if chars > PREVIEW_CHARS { "\n..." } else { "" };
    format!(
        "<persisted-output>\n\
         Output too large ({chars} characters). Full output saved to: {path}\n\
         Read that file in parts or search it to see the rest; do not re-run the call to get it again.\n\
         \n\
         Preview (first {PREVIEW_CHARS} characters):\n\
         {preview}{more}\n\
         </persisted-output>",
        path = path.display()
    )
}

/// A call id as a file name: ASCII letters, digits, `-` and `_` only.
fn file_safe(call_id: &str) -> String {
    let safe: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    if safe.is_empty() { "call".into() } else { safe }
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
