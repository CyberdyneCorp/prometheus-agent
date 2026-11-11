//! Cursor (`~/.cursor`; spec: hermes-migration, Claude Code, Codex CLI
//! and Cursor mappings): `AGENTS.md` and `rules/*.md|*.mdc` (front matter
//! stripped) to agent memory, skills at any depth under `skills/` to
//! `skills/cursor-imports/` and `mcp.json` to MCP servers.
//! `cli-config.json` is never read.

use pmt_store::{MemoryKind, MemoryTarget};
use serde_json::Value;

use super::build::Build;
use super::mcp::Secrets;
use crate::component::Component;

pub(super) fn plan(b: &mut Build<'_>) {
    let root = b.reader.root().to_owned();
    b.memory_file(
        &root.join("AGENTS.md"),
        MemoryTarget::Memory,
        MemoryKind::Instruction,
        false,
    );
    for file in b.reader.files(&root.join("rules"), &["md", "mdc"]) {
        b.memory_file(&file, MemoryTarget::Memory, MemoryKind::Instruction, false);
    }
    b.skills_in(&root.join("skills"), true);
    if let Some(servers) = b
        .json(Component::Mcp, &root.join("mcp.json"))
        .as_ref()
        .and_then(|d| d.get("mcpServers"))
        .and_then(Value::as_object)
    {
        b.mcp_servers("mcp.json:mcpServers", &servers.clone(), Secrets::Strip);
    }
}
