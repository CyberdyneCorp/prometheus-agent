//! What the desktop's context meter lists (spec: desktop-app, Desktop
//! status bar): the session's context by category, estimated like the
//! offline prompt size report (4 bytes a token). The system prompt is
//! rebuilt from the session's own inputs to measure its layers; the
//! frozen prompt is neither replaced nor re-stored.

use pmt_core::error::{Error, codes};

use super::Agent;
use super::model::RouteInfo;
use crate::prompt::size::{report, tokens};
use crate::prompt::{self, Layer};

/// The delegation tool, whose schema defines the subagents.
const DELEGATE_TOOL: &str = "delegate_task";

/// The categories in display order.
pub const CATEGORIES: [&str; 8] = [
    "system_prompt",
    "tool_definitions",
    "skills",
    "memory",
    "rules",
    "mcp",
    "subagent_definitions",
    "conversation",
];

impl Agent {
    /// Estimated tokens of each of [`CATEGORIES`]: the prompt's layers on
    /// `route`, the tool schemas and `conversation_bytes` of messages.
    ///
    /// # Errors
    ///
    /// When the prompt cannot be built (a profile file became unreadable).
    pub async fn context_breakdown(
        &self,
        config: &pmt_config::Config,
        route: &RouteInfo,
        conversation_bytes: usize,
    ) -> Result<Vec<(&'static str, u64)>, Error> {
        let tools = self.setup.tools.definitions();
        let inputs = self.prompt_inputs(
            config,
            route,
            tools.iter().map(|t| t.name.clone()).collect(),
        );
        let built = pmt_core::spawn::blocking(move || prompt::build(&inputs))
            .await
            .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("prompt build: {e}")))??;
        let mut bytes = [0usize; CATEGORIES.len()];
        let layers = report(&built.prompt, &[], "").layers;
        for (layer, size) in Layer::ALL.iter().zip(layers) {
            bytes[layer_category(*layer)] += size.bytes;
        }
        for tool in &tools {
            let schema = serde_json::to_string(&serde_json::json!({
                "name": tool.name, "description": tool.description, "parameters": tool.parameters,
            }))
            .map_or(0, |s| s.len());
            bytes[tool_category(&tool.name)] += schema;
        }
        bytes[CATEGORIES.len() - 1] = conversation_bytes;
        Ok(CATEGORIES
            .iter()
            .zip(bytes)
            .map(|(name, b)| (*name, u64::try_from(tokens(b)).unwrap_or(u64::MAX)))
            .collect())
    }
}

/// The index in [`CATEGORIES`] of a prompt layer.
const fn layer_category(layer: Layer) -> usize {
    match layer {
        Layer::SkillsIndex | Layer::PinnedSkills => 2,
        Layer::Memory => 3,
        Layer::ProjectContext => 4,
        _ => 0,
    }
}

/// The index in [`CATEGORIES`] of a tool's schema.
fn tool_category(name: &str) -> usize {
    if name.starts_with("mcp__") {
        5
    } else if name == DELEGATE_TOOL {
        6
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::{CATEGORIES, tool_category};

    #[test]
    fn mcp_and_delegation_tools_have_their_own_categories() {
        assert_eq!(CATEGORIES[tool_category("mcp__gh__issue")], "mcp");
        assert_eq!(
            CATEGORIES[tool_category("delegate_task")],
            "subagent_definitions"
        );
        assert_eq!(CATEGORIES[tool_category("read_file")], "tool_definitions");
    }
}
