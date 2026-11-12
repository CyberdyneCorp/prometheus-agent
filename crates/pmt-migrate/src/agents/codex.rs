//! Codex CLI (`~/.codex`; spec: hermes-migration, Claude Code, Codex CLI
//! and Cursor mappings): `AGENTS.md` and `memories/*.md` to agent memory,
//! `skills/` to `skills/codex-imports/` and `config.toml`
//! `[mcp_servers.*]` to MCP servers. `auth.json` is never read.

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
    for file in b.reader.files(&root.join("memories"), &["md"]) {
        b.memory_file(&file, MemoryTarget::Memory, MemoryKind::Fact, false);
    }
    b.skills_in(&root.join("skills"), false);
    let path = root.join("config.toml");
    match b.reader.toml(&path) {
        Ok(Some(config)) => {
            if let Some(servers) = config.get("mcp_servers").and_then(Value::as_object) {
                b.mcp_servers("config.toml:mcp_servers", &servers.clone(), Secrets::Strip);
            }
        }
        Ok(None) => {}
        Err(why) => b.broken(Component::Mcp, "config.toml".to_owned(), why),
    }
}
