//! The offline prompt size report (spec: agent-turn-loop, Offline prompt
//! size report): bytes and estimated tokens of every prompt layer and of
//! each toolset's tool schemas, summing exactly to what a new session
//! sends as its system blocks and tool array.

use serde::Serialize;

use super::{Layer, SEPARATOR, SystemPrompt};

/// Estimated tokens of `bytes` (4 bytes per token, rounded up).
#[must_use]
pub const fn tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// One prompt layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerSize {
    /// Layer name.
    pub layer: &'static str,
    /// Its tier.
    pub tier: &'static str,
    /// Bytes, including the separator before it within its tier.
    pub bytes: usize,
    /// Estimated tokens.
    pub tokens: usize,
}

/// One toolset's schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolsetSize {
    /// Toolset name.
    pub toolset: String,
    /// Its exposed tools.
    pub tools: Vec<String>,
    /// Bytes of their serialized schemas.
    pub bytes: usize,
    /// Estimated tokens.
    pub tokens: usize,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizeReport {
    /// Every layer in prompt order (empty layers with 0 bytes).
    pub layers: Vec<LayerSize>,
    /// Tool schemas grouped by toolset, in tool order of first appearance.
    pub toolsets: Vec<ToolsetSize>,
    /// Bytes of the system blocks.
    pub system_bytes: usize,
    /// Bytes of the tool array as sent.
    pub tools_bytes: usize,
    /// Array brackets and commas of the tool array.
    pub tools_framing_bytes: usize,
    /// System and tool bytes.
    pub total_bytes: usize,
    /// Estimated tokens of the total.
    pub total_tokens: usize,
}

/// One exposed tool: its toolset and serialized schema (as it appears in
/// the tool array).
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// Tool name.
    pub name: String,
    /// Its toolset.
    pub toolset: String,
    /// The schema's JSON text.
    pub json: String,
}

/// Measures `prompt` and the tool array `tools_json` built from `tools`.
#[must_use]
pub fn report(prompt: &SystemPrompt, tools: &[ToolSchema], tools_json: &str) -> SizeReport {
    let layers = layer_sizes(prompt);
    let system_bytes = layers.iter().map(|l| l.bytes).sum();
    let toolsets = toolset_sizes(tools);
    let schema_bytes: usize = toolsets.iter().map(|t| t.bytes).sum();
    let tools_bytes = tools_json.len();
    let total_bytes = system_bytes + tools_bytes;
    SizeReport {
        layers,
        toolsets,
        system_bytes,
        tools_bytes,
        tools_framing_bytes: tools_bytes.saturating_sub(schema_bytes),
        total_bytes,
        total_tokens: tokens(total_bytes),
    }
}

fn layer_sizes(prompt: &SystemPrompt) -> Vec<LayerSize> {
    let mut previous_tier = None;
    let mut sizes = Vec::new();
    for layer in Layer::ALL {
        let section = prompt.sections().iter().find(|s| s.layer == layer);
        let bytes = section.map_or(0, |s| {
            let joined = previous_tier == Some(layer.tier());
            previous_tier = Some(layer.tier());
            s.text.len() + if joined { SEPARATOR.len() } else { 0 }
        });
        sizes.push(LayerSize {
            layer: layer.as_str(),
            tier: layer.tier().as_str(),
            bytes,
            tokens: tokens(bytes),
        });
    }
    sizes
}

fn toolset_sizes(tools: &[ToolSchema]) -> Vec<ToolsetSize> {
    let mut sets: Vec<ToolsetSize> = Vec::new();
    for tool in tools {
        let index = sets
            .iter()
            .position(|s| s.toolset == tool.toolset)
            .unwrap_or_else(|| {
                sets.push(ToolsetSize {
                    toolset: tool.toolset.clone(),
                    tools: Vec::new(),
                    bytes: 0,
                    tokens: 0,
                });
                sets.len() - 1
            });
        let set = &mut sets[index];
        set.tools.push(tool.name.clone());
        set.bytes += tool.json.len();
        set.tokens = tokens(set.bytes);
    }
    sets
}
