//! The context window view behind `/context` (spec: context-compression,
//! Context window view).
//!
//! Everything here is computed locally from the session's own state: the
//! prompt layers are rebuilt in memory from the same inputs the frozen
//! prompt was built from (nothing is stored, sent or refrozen, so the
//! prompt cache is untouched), tool schemas come from the session's tool
//! surface and the conversation from its active rows. Token counts are
//! the usual 4-bytes-a-token estimates.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_store::{UsageScope, Visibility};

use super::Agent;
use super::compression::window;
use crate::compress::{is_summary, rows_tokens};
use crate::prompt::inventory::{ContextFile, inventory};
use crate::prompt::{self, Layer};

/// One category of the window and its estimated tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// `system prompt`, `tool definitions`, ...
    pub name: &'static str,
    /// Its glyph in the grid.
    pub glyph: char,
    /// Estimated tokens.
    pub tokens: u64,
}

/// One toolset's schema cost (`/context all`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsetCost {
    /// The toolset.
    pub toolset: String,
    /// Its exposed tools.
    pub tools: usize,
    /// Estimated tokens of their schemas.
    pub tokens: u64,
}

/// One skill's cost (`/context all`), filled in by the caller, which
/// knows the skills: its line in the index versus its full load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCost {
    /// The skill.
    pub name: String,
    /// Tokens of its skills-index entry.
    pub index_tokens: u64,
    /// Tokens of its whole `SKILL.md`.
    pub full_tokens: u64,
}

/// The whole view.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    /// The model's window.
    pub window: u64,
    /// Every category in display order, `free` last.
    pub categories: Vec<Category>,
    /// The compaction trigger.
    pub trigger_tokens: u64,
    /// Committed compactions.
    pub compactions: u32,
    /// When the latest one committed.
    pub last_compaction: Option<Timestamp>,
    /// Input tokens processed by the session so far (cache included).
    pub input_tokens: u64,
    /// Output tokens produced so far.
    pub output_tokens: u64,
    /// The candidate context files.
    pub files: Vec<ContextFile>,
    /// Per-toolset schema costs.
    pub toolsets: Vec<ToolsetCost>,
    /// Per-skill costs (empty unless the caller fills them in).
    pub skills: Vec<SkillCost>,
}

impl ContextView {
    /// Tokens in use (every category but `free`).
    #[must_use]
    pub fn used(&self) -> u64 {
        self.categories
            .iter()
            .filter(|c| c.name != FREE)
            .map(|c| c.tokens)
            .sum()
    }
}

const FREE: &str = "free";

/// Which category a prompt layer counts in.
fn layer_category(layer: Layer) -> usize {
    match layer {
        Layer::ProjectContext => 2,
        Layer::SkillsIndex | Layer::PinnedSkills => 3,
        Layer::Memory => 6,
        _ => 0,
    }
}

/// The toolset whose schemas count as `subagents`.
const DELEGATION: &str = "delegation";

impl Agent {
    /// The context window view of this session.
    ///
    /// # Errors
    ///
    /// Store failures; `task.panicked` when the prompt build panics.
    pub async fn context_view(&self) -> Result<ContextView, Error> {
        let config = self.setup.scope.config();
        let route = self.setup.model.route().await;
        let tools = self.setup.tools.definitions();
        let inputs = self.prompt_inputs(
            config,
            &route,
            tools.iter().map(|t| t.name.clone()).collect(),
        );
        let (built, files) = pmt_core::spawn::blocking(move || {
            prompt::build(&inputs).map(|built| (built, inventory(&inputs)))
        })
        .await
        .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("context view: {e}")))??;
        let names = [
            ("system prompt", 's'),
            ("tool definitions", 't'),
            ("project rules", 'p'),
            ("skills index", 'k'),
            ("MCP tools", 'm'),
            ("subagents", 'a'),
            ("memory", 'y'),
            ("conversation", 'c'),
        ];
        let mut categories: Vec<Category> = names
            .iter()
            .map(|&(name, glyph)| Category {
                name,
                glyph,
                tokens: 0,
            })
            .collect();
        for section in built.prompt.sections() {
            categories[layer_category(section.layer)].tokens +=
                crate::compress::tokens(&section.text);
        }
        let mut toolsets: Vec<ToolsetCost> = Vec::new();
        for tool in &tools {
            let json = serde_json::to_string(tool).unwrap_or_default();
            let cost = crate::compress::tokens(&json);
            let (toolset, mcp) = self.setup.tools.toolset_of(&tool.name).unwrap_or_default();
            let at = match (mcp, toolset.as_str()) {
                (true, _) => 4,
                (false, DELEGATION) => 5,
                _ => 1,
            };
            categories[at].tokens += cost;
            match toolsets.iter_mut().find(|t| t.toolset == toolset) {
                Some(set) => {
                    set.tools += 1;
                    set.tokens += cost;
                }
                None => toolsets.push(ToolsetCost {
                    toolset,
                    tools: 1,
                    tokens: cost,
                }),
            }
        }
        let store = &self.setup.store;
        let session = self.setup.session;
        let rows = store.messages(session, Visibility::Active)?;
        categories[7].tokens = rows_tokens(&rows);
        let window_tokens = window(&route).context_window;
        let used: u64 = categories.iter().map(|c| c.tokens).sum();
        categories.push(Category {
            name: FREE,
            glyph: '.',
            tokens: window_tokens.saturating_sub(used),
        });
        let totals = store.usage_totals(UsageScope::Session(session))?;
        let t = totals.tokens;
        Ok(ContextView {
            window: window_tokens,
            categories,
            trigger_tokens: self.trigger_tokens(config, &route),
            compactions: store.session(session)?.compaction_count,
            last_compaction: rows
                .iter()
                .filter(|r| is_summary(r))
                .map(|r| r.created_at)
                .max(),
            input_tokens: t.input + t.cache_read + t.cache_write,
            output_tokens: t.output,
            files,
            toolsets,
            skills: Vec::new(),
        })
    }
}
