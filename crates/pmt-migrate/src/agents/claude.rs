//! Claude Code (`~/.claude`; spec: hermes-migration, Claude Code, Codex
//! CLI and Cursor mappings): `CLAUDE.md` to agent memory, `skills/` to
//! `skills/claude-code-imports/`, MCP servers from `~/.claude.json` and
//! `settings.json`, `Bash(...)` permission rules to command approvals and
//! deny rules. `commands/*.md` and other permission rules are reported
//! unsupported. `.credentials.json` is never read.

use std::path::PathBuf;

use pmt_store::{MemoryKind, MemoryTarget};
use serde_json::Value;

use super::build::Build;
use super::mcp::Secrets;
use crate::component::Component;
use crate::report::{Disposition, Entry};

pub(super) fn plan(b: &mut Build<'_>) {
    let root = b.reader.root().to_owned();
    b.memory_file(
        &root.join("CLAUDE.md"),
        MemoryTarget::Memory,
        MemoryKind::Instruction,
        false,
    );
    b.skills_in(&root.join("skills"), false);
    // Claude Code rewrites `~/.claude.json` on most launches: only its
    // `mcpServers` count, so the rest never triggers a sync.
    if let Some(state) = state_file(b)
        && let Some(servers) = b
            .json_part(Component::Mcp, &state, "mcpServers")
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
    {
        b.mcp_servers(".claude.json:mcpServers", &servers, Secrets::Strip);
    }
    if let Some(settings) = b.json(Component::Config, &root.join("settings.json")) {
        if let Some(servers) = servers(&settings) {
            b.mcp_servers("settings.json:mcpServers", &servers, Secrets::Strip);
        }
        permissions(b, &settings);
    }
    for command in b.reader.files(&root.join("commands"), &["md"]) {
        let item = b.item(&command);
        b.note(
            Entry::new(Component::Skills, item, Disposition::Unsupported)
                .because("Claude Code slash commands have no Prometheus equivalent; convert the command into a skill"),
        );
    }
}

/// `~/.claude.json`: inside a relocated config directory, else beside
/// `~/.claude`.
fn state_file(b: &Build<'_>) -> Option<PathBuf> {
    let root = b.reader.root();
    let inside = root.join(".claude.json");
    if inside.is_file() {
        return Some(inside);
    }
    let name = root.file_name()?.to_string_lossy().into_owned();
    Some(root.parent()?.join(format!("{name}.json"))).filter(|p| p.is_file())
}

fn servers(doc: &Value) -> Option<serde_json::Map<String, Value>> {
    doc.get("mcpServers").and_then(Value::as_object).cloned()
}

/// `Bash(npm run test:*)` -> `npm run test*`; `Bash(git status)` ->
/// `git status`; `None` for a rule that is not a `Bash(...)` command.
#[must_use]
pub(super) fn bash_pattern(rule: &str) -> Option<String> {
    let inner = rule.trim().strip_prefix("Bash(")?.strip_suffix(')')?.trim();
    if inner.is_empty() {
        return None;
    }
    Some(match inner.strip_suffix(":*") {
        Some(prefix) => format!("{prefix}*"),
        None => inner.to_owned(),
    })
}

fn permissions(b: &mut Build<'_>, settings: &Value) {
    let Some(perms) = settings.get("permissions") else {
        return;
    };
    for (list, allow) in [("allow", true), ("deny", false)] {
        let rules = perms
            .get(list)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for rule in rules.iter().filter_map(Value::as_str) {
            let item = format!("settings.json:permissions.{list}:{rule}");
            let entry = |d| Entry::new(Component::Config, item.clone(), d);
            match bash_pattern(rule) {
                Some(pattern) if allow => {
                    let e = entry(Disposition::Transformed)
                        .to("grants.toml")
                        .because(format!("permanent command approval `{pattern}`"));
                    b.allow(e, pattern);
                }
                Some(pattern) => {
                    let e = entry(Disposition::Transformed)
                        .to("approvals.deny")
                        .because(format!("deny rule `{pattern}`"));
                    b.deny(e, pattern);
                }
                None => b.note(entry(Disposition::Unsupported).because(
                    "only Bash(...) command rules map to Prometheus approvals; review the tool's approval settings",
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bash_pattern;

    #[test]
    fn bash_rules_become_command_patterns() {
        assert_eq!(
            bash_pattern("Bash(npm run test:*)").as_deref(),
            Some("npm run test*")
        );
        assert_eq!(
            bash_pattern("Bash(git status)").as_deref(),
            Some("git status")
        );
        assert_eq!(bash_pattern("WebFetch"), None);
        assert_eq!(bash_pattern("Bash"), None);
    }
}
