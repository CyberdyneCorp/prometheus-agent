//! MCP server definitions of other agents (Claude Code and Cursor
//! `mcpServers`, Codex `[mcp_servers.*]`, OpenClaw `mcp.servers`) to
//! `mcp.servers.<name>` (spec: hermes-migration, Other agents never yield
//! secrets by default).
//!
//! Every environment variable or header whose name looks like a secret
//! (`*_TOKEN`, `*_KEY`, `*_SECRET`, `*PASSWORD*`, `Authorization`) is
//! stripped: the server references the secret by name (`env_secrets`, a
//! `${NAME}` header placeholder) and the report lists it as `secret` with
//! the command that adds it. Only an OpenClaw import with
//! `--migrate-secrets` carries the values, into the secrets store.

use pmt_config::schema::McpServerConfig;
use serde_json::{Map, Value, json};

use crate::component::Component;
use crate::config::{json_to_toml, refs};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

/// Whether `name` is a secret by the importers' pattern.
#[must_use]
pub fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "AUTHORIZATION"
        || upper.contains("PASSWORD")
        || ["_TOKEN", "_KEY", "_SECRET"]
            .iter()
            .any(|s| upper.ends_with(s))
}

/// How stripped secrets are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secrets {
    /// Named only (the default for every agent).
    Strip,
    /// Written to the secrets store (OpenClaw `--migrate-secrets`).
    Import,
}

/// What one server became.
#[derive(Default)]
struct Converted {
    table: Map<String, Value>,
    /// Secret names (and values, when present) the server references.
    secrets: Vec<(String, Option<String>)>,
    notes: Vec<String>,
}

/// Plans every server of `servers` (`item` prefixes the report items,
/// like `mcpServers`), adding entries and writes.
pub fn plan(
    item: &str,
    servers: &Map<String, Value>,
    secrets: Secrets,
    entries: &mut Vec<Entry>,
    writes: &mut Writes,
) {
    for (name, server) in servers {
        plan_one(
            &format!("{item}.{name}"),
            name,
            server,
            secrets,
            entries,
            writes,
        );
    }
}

fn plan_one(
    item: &str,
    name: &str,
    server: &Value,
    secrets: Secrets,
    entries: &mut Vec<Entry>,
    writes: &mut Writes,
) {
    let target = format!("mcp.servers.{name}");
    let entry = |d| Entry::new(Component::Mcp, item.to_owned(), d).to(&target);
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let Some(fields) = server.as_object().filter(|_| plain) else {
        entries
            .push(entry(Disposition::Failed).because("not a server definition with a plain name"));
        return;
    };
    let converted = convert(name, fields);
    if let Err(e) =
        serde_json::from_value::<McpServerConfig>(Value::Object(converted.table.clone()))
    {
        entries.push(
            entry(Disposition::Failed).because(format!("Prometheus refuses the server: {e}")),
        );
        return;
    }
    let Some(value) = json_to_toml(&Value::Object(converted.table)) else {
        entries.push(entry(Disposition::Failed).because("not representable in config.toml"));
        return;
    };
    let index = entries.len();
    entries.push(if converted.notes.is_empty() {
        entry(Disposition::Mapped)
    } else {
        entry(Disposition::Transformed).because(converted.notes.join("; "))
    });
    writes.key(&target, value, index);
    for (secret, value) in converted.secrets {
        let e = Entry::new(
            Component::Mcp,
            format!("{item}:{secret}"),
            Disposition::Secret,
        )
        .to(&secret);
        match (secrets, value) {
            (Secrets::Import, Some(v)) => {
                writes.secret(&secret, &v, entries.len());
                entries.push(e.because("stored in the secrets store"));
            }
            _ => entries.push(e.because(format!(
                "value not imported; add it with `prometheus secrets set {secret}`"
            ))),
        }
    }
}

fn convert(server: &str, fields: &Map<String, Value>) -> Converted {
    let mut out = Converted::default();
    let mut dropped = Vec::new();
    for (key, value) in fields {
        match key.as_str() {
            "command" | "args" | "url" => {
                out.table.insert(key.clone(), value.clone());
            }
            "type" | "transport" => transport(&mut out, value),
            "env" => environment(&mut out, value),
            "headers" | "http_headers" => headers(server, &mut out, value),
            "bearer_token_env_var" => bearer(&mut out, value),
            "disabled" => {
                out.table
                    .insert("enabled".to_owned(), json!(value.as_bool() != Some(true)));
            }
            "enabled" => {
                out.table.insert("enabled".to_owned(), value.clone());
            }
            "enabled_tools" | "include" => filter(&mut out, "include", value),
            "disabled_tools" | "exclude" => filter(&mut out, "exclude", value),
            "tools" => {
                for k in ["include", "exclude"] {
                    if let Some(list) = value.get(k) {
                        filter(&mut out, k, list);
                    }
                }
            }
            "startup_timeout_sec" => seconds(&mut out, "connect_timeout_secs", value),
            "tool_timeout_sec" | "timeout" => seconds(&mut out, "timeout_secs", value),
            other => dropped.push(other.to_owned()),
        }
    }
    if out.table.contains_key("url") && !out.table.contains_key("transport") {
        out.table.insert("transport".to_owned(), json!("http"));
    }
    if !dropped.is_empty() {
        out.notes
            .push(format!("not supported, dropped: {}", dropped.join(", ")));
    }
    out
}

fn transport(out: &mut Converted, value: &Value) {
    let kind = match value.as_str() {
        Some("stdio") => "stdio",
        Some("sse") => {
            out.notes
                .push("SSE transport served as streamable HTTP".to_owned());
            "http"
        }
        _ => "http",
    };
    out.table.insert("transport".to_owned(), json!(kind));
}

fn filter(out: &mut Converted, key: &str, value: &Value) {
    if value.is_array() {
        let tools = out
            .table
            .entry("tools".to_owned())
            .or_insert_with(|| json!({}));
        tools[key] = value.clone();
    }
}

fn seconds(out: &mut Converted, key: &str, value: &Value) {
    if let Some(n) = value.as_f64() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let secs = n.ceil().max(1.0) as u64;
        out.table.insert(key.to_owned(), json!(secs));
    }
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn environment(out: &mut Converted, value: &Value) {
    let Some(vars) = value.as_object() else {
        return;
    };
    let mut env = Map::new();
    let mut env_secrets = Vec::new();
    for (key, raw) in vars {
        let value = text(raw);
        if secret_name(key) {
            // `env_secrets` passes the secrets-store entry of that name.
            let literal = refs::whole(&value).is_none().then_some(value);
            out.secrets.push((key.clone(), literal));
            env_secrets.push(key.clone());
        } else {
            env.insert(key.clone(), json!(value));
        }
    }
    if !env.is_empty() {
        out.table.insert("env".to_owned(), Value::Object(env));
    }
    if !env_secrets.is_empty() {
        out.notes
            .push(format!("credentials stripped: {}", env_secrets.join(", ")));
        out.table
            .insert("env_secrets".to_owned(), json!(env_secrets));
    }
}

fn headers(server: &str, out: &mut Converted, value: &Value) {
    let Some(fields) = value.as_object() else {
        return;
    };
    let mut kept = Map::new();
    for (name, raw) in fields {
        let value = text(raw);
        if !secret_name(name) {
            kept.insert(name.clone(), json!(refs::as_placeholders(&value)));
            continue;
        }
        let (scheme, token) = match value.split_once(' ') {
            Some((s, t)) if s.eq_ignore_ascii_case("bearer") => (format!("{s} "), t.to_owned()),
            _ => (String::new(), value),
        };
        let (secret, literal) = match refs::whole(&token) {
            Some(var) => (var, None),
            None => (derived(server, name), Some(token)),
        };
        out.notes.push(format!("header {name} stripped"));
        kept.insert(name.clone(), json!(format!("{scheme}${{{secret}}}")));
        out.secrets.push((secret, literal));
    }
    out.table.insert("headers".to_owned(), Value::Object(kept));
}

fn bearer(out: &mut Converted, value: &Value) {
    if let Some(var) = value.as_str() {
        out.table.insert("auth_secret".to_owned(), json!(var));
        out.secrets.push((var.to_owned(), None));
    }
}

/// The secret name of a literal credential header of `server`.
fn derived(server: &str, header: &str) -> String {
    format!("MCP_{server}_{header}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_follow_the_pattern() {
        for name in [
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "app_secret",
            "DB_PASSWORD_FILE",
            "Authorization",
        ] {
            assert!(secret_name(name), "{name}");
        }
        for name in ["PATH", "TOKENIZER", "KEYBOARD", "X-Request-Id"] {
            assert!(!secret_name(name), "{name}");
        }
    }

    #[test]
    fn a_token_is_stripped_and_named() {
        let servers =
            json!({"github": {"command": "npx", "env": {"GITHUB_TOKEN": "ghp_x", "LOG": "1"}}});
        let mut entries = Vec::new();
        let mut writes = Writes::default();
        plan(
            "mcpServers",
            servers.as_object().unwrap(),
            Secrets::Strip,
            &mut entries,
            &mut writes,
        );
        assert!(writes.secrets.is_empty());
        let table = writes.config[0].value.to_string();
        assert!(
            table.contains("env_secrets") && table.contains("GITHUB_TOKEN"),
            "{table}"
        );
        assert!(!table.contains("ghp_x"), "{table}");
        let secret = entries
            .iter()
            .find(|e| e.disposition == Disposition::Secret)
            .unwrap();
        assert!(
            secret
                .reason
                .contains("prometheus secrets set GITHUB_TOKEN")
        );
    }
}
