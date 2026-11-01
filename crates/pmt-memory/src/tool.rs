//! The `memory` tool's registry entry (spec: memory, Memory tool surface;
//! Store enable flags and tool availability).
//!
//! The turn loop executes it ([`crate::MemorySession`] behind
//! `pmt_agent::tools::MemoryPort`); the entry carries the argument
//! contract, an opt-in probe (offered only while a target is enabled) and
//! a schema rewriter that advertises only the enabled targets.

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_providers::request::Tool;
use pmt_tools::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};
use pmt_tools::probe::FnProbe;
use serde_json::{Value, json};

use crate::budget;

/// The tool's name.
pub const NAME: &str = "memory";

const DESCRIPTION: &str = "Save durable knowledge that should outlive this session. target \
\"user\" holds who the user is (preferences, role, habits, how they like to work); target \
\"memory\" holds your own notes (environment facts, project conventions, lessons learned). Send \
every change for one target in one call as `operations`: add (content, optional kind and tier), \
patch (the entry's id, or an old_text that matches exactly one entry, plus the full new content), \
remove, pin, unpin. Core entries are shown in the system prompt of future sessions and share a \
character budget; tier \"archival\" entries have no budget and are recalled into a turn when \
relevant. The current system prompt does not change: this session sees your changes only in \
tool results. Never store secrets, task progress or anything the user asked to keep private.";

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        NAME,
        "memory",
        DESCRIPTION,
        json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "enum": ["memory", "user"], "description": "Which store the operations apply to."},
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "action": {"type": "string", "enum": ["add", "patch", "remove", "pin", "unpin"]},
                            "id": {"type": "string", "description": "The entry's id (m_...)."},
                            "old_text": {"type": "string", "description": "A substring of exactly one entry, when the id is unknown."},
                            "content": {"type": "string", "description": "The entry's full text (add, patch)."},
                            "kind": {"type": "string", "enum": ["fact", "preference", "instruction", "project", "person", "event"]},
                            "tier": {"type": "string", "enum": ["core", "archival"]},
                            "expires_at": {"type": "string", "format": "date-time", "description": "RFC 3339 time after which the entry no longer applies."}
                        },
                        "required": ["action"]
                    }
                }
            },
            "required": ["target", "operations"]
        }),
    );
    d.effect = Effect::None;
    // Batches read and write the profile's entries: calls of one model
    // response apply in order.
    d.claims = Some(ResourceClaims::exclusive("memory"));
    d
}

/// `parameters` with the target enum narrowed to what `config` enables.
#[must_use]
pub fn parameters(parameters: &Value, config: &Config) -> Value {
    let mut narrowed = parameters.clone();
    let targets: Vec<&str> = budget::enabled(&config.memory)
        .into_iter()
        .map(pmt_store::MemoryTarget::as_str)
        .collect();
    if let Some(slot) = narrowed.pointer_mut("/properties/target/enum") {
        *slot = json!(targets);
    }
    narrowed
}

/// The registry entry.
#[must_use]
pub fn registered() -> RegisteredTool {
    let probe = FnProbe::new(
        "memory_enabled",
        |ctx: pmt_tools::probe::ProbeContext<'_>| {
            if budget::enabled(&ctx.config.memory).is_empty() {
                Err("memory disabled (memory.memory_enabled and memory.user_profile_enabled are false)".to_owned())
            } else {
                Ok(())
            }
        },
    );
    RegisteredTool::agent_loop(descriptor())
        .with_probe(Arc::new(probe))
        .with_parameters(Arc::new(parameters))
}

/// The tool as a model request sees it under `config` (background reviews
/// that run the memory tool alone).
#[must_use]
pub fn definition(config: &Config) -> Tool {
    let d = descriptor();
    Tool {
        name: d.name.clone(),
        description: d.description.clone(),
        parameters: parameters(&d.parameters, config),
    }
}
