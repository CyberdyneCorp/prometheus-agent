//! The `mcp` component (spec: hermes-migration, MCP servers and plugins;
//! Config references to environment values).
//!
//! Each Hermes `mcp_servers` entry becomes `[mcp.servers.<name>]`.
//! Credential-bearing values move to the secrets store: a `${NAME}`
//! header stays a placeholder of the secret `NAME`, a literal credential
//! header or environment value is stored under a derived name and
//! referenced, and a stdio server's secret environment variables become
//! `env_secrets`.

use pmt_config::schema::McpServerConfig;
use pmt_core::error::Error;
use serde_json::{Map, Value, json};

use crate::component::Component;
use crate::config::{json_to_toml, refs};
use crate::env::{self, Class};
use crate::plan::{Dest, Existing, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

/// Hermes server keys copied as they are.
const SAME: &[&str] = &[
    "command",
    "args",
    "url",
    "enabled",
    "lazy",
    "tool_costs",
    "spend_limit",
    "cost_unit",
    "client_cert",
    "client_key",
    "identity_header",
];

/// The planned MCP component.
pub struct McpStep {
    entries: Vec<Entry>,
    writes: Writes,
}

impl Step for McpStep {
    fn component(&self) -> Component {
        Component::Mcp
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn predict(&mut self, existing: &Existing<'_>) {
        self.writes.predict(existing, &mut self.entries);
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        self.writes.apply(dest, &mut self.entries)
    }
}

/// Plans the MCP component.
#[must_use]
pub fn plan(src: &Source<'_>) -> McpStep {
    let mut step = McpStep {
        entries: Vec::new(),
        writes: Writes::default(),
    };
    let Some(servers) = src.get("mcp_servers").and_then(Value::as_object) else {
        return step;
    };
    for (name, server) in servers {
        plan_server(src, name, server, &mut step);
    }
    step
}

/// What one server became.
#[derive(Default)]
struct Converted {
    table: Map<String, Value>,
    secrets: Vec<(String, String)>,
    notes: Vec<String>,
}

fn plan_server(src: &Source<'_>, name: &str, server: &Value, step: &mut McpStep) {
    let item = format!("mcp_servers.{name}");
    let target = format!("mcp.servers.{name}");
    let entry = |d| Entry::new(Component::Mcp, item.clone(), d).to(&target);
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let Some(fields) = server.as_object().filter(|_| valid_name) else {
        step.entries
            .push(entry(Disposition::Failed).because("not a server definition with a plain name"));
        return;
    };
    let converted = convert(src, name, fields);
    if let Err(e) =
        serde_json::from_value::<McpServerConfig>(Value::Object(converted.table.clone()))
    {
        step.entries.push(
            entry(Disposition::Failed).because(format!("Prometheus refuses the server: {e}")),
        );
        return;
    }
    let Some(value) = json_to_toml(&Value::Object(converted.table)) else {
        step.entries
            .push(entry(Disposition::Failed).because("not representable in config.toml"));
        return;
    };
    let index = step.entries.len();
    let e = if converted.notes.is_empty() {
        entry(Disposition::Mapped)
    } else {
        entry(Disposition::Transformed).because(converted.notes.join("; "))
    };
    step.entries.push(e);
    step.writes.key(target, value, index);
    for (secret, value) in converted.secrets {
        step.writes.secret(secret, &value, index);
    }
}

fn convert(src: &Source<'_>, server: &str, fields: &Map<String, Value>) -> Converted {
    let mut out = Converted::default();
    let mut dropped = Vec::new();
    for (key, value) in fields {
        match key.as_str() {
            k if SAME.contains(&k) => {
                out.table.insert(k.to_owned(), value.clone());
            }
            "timeout" => seconds(&mut out, "timeout_secs", value),
            "connect_timeout" => seconds(&mut out, "connect_timeout_secs", value),
            "transport" => transport(&mut out, value),
            "auth" if value.as_str() == Some("oauth") => {
                out.table.insert("auth".to_owned(), json!("oauth"));
            }
            "tools" => tools(&mut out, value, &mut dropped),
            "env" => environment(src, &mut out, value),
            "headers" => headers(src, server, &mut out, value),
            other => dropped.push(other.to_owned()),
        }
    }
    let http = out.table.contains_key("url");
    if http && !out.table.contains_key("transport") {
        out.table.insert("transport".to_owned(), json!("http"));
    }
    if !dropped.is_empty() {
        out.notes
            .push(format!("not supported, dropped: {}", dropped.join(", ")));
    }
    out
}

fn seconds(out: &mut Converted, key: &str, value: &Value) {
    if let Some(n) = value.as_f64() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let secs = n.ceil().max(1.0) as u64;
        out.table.insert(key.to_owned(), json!(secs));
    }
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

fn tools(out: &mut Converted, value: &Value, dropped: &mut Vec<String>) {
    let mut filter = Map::new();
    for key in ["include", "exclude"] {
        if let Some(list) = value.get(key) {
            filter.insert(key.to_owned(), list.clone());
        }
    }
    if value
        .as_object()
        .is_some_and(|m| m.keys().any(|k| k != "include" && k != "exclude"))
    {
        dropped.push("tools (other than include/exclude)".to_owned());
    }
    out.table.insert("tools".to_owned(), Value::Object(filter));
}

/// `MCP_<SERVER>_<NAME>` in secret-name form.
fn derived(server: &str, name: &str) -> String {
    format!("MCP_{server}_{name}")
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

fn is_secret(src: &Source<'_>, name: &str) -> bool {
    matches!(
        env::classify(name, &|n| src.host.known_secret(n)),
        Class::Secret | Class::Unclassified
    )
}

/// Where one environment variable of a stdio server goes.
enum EnvVar {
    /// A literal value in `env`.
    Plain(String),
    /// A secret passed under its own name (`env_secrets`).
    Secret,
    /// Nothing (an unresolved reference).
    Dropped,
}

fn env_var(src: &Source<'_>, out: &mut Converted, key: &str, text: String) -> EnvVar {
    let Some(name) = refs::whole(&text) else {
        if env::secret_shaped(key) {
            // `env_secrets` passes a secret under its own name.
            out.secrets.push((key.to_owned(), text));
            return EnvVar::Secret;
        }
        return EnvVar::Plain(text);
    };
    let value = src.env_value(&name);
    if value.is_none() {
        out.notes.push(format!("env {key}: unresolved ${{{name}}}"));
    }
    if is_secret(src, &name) || env::secret_shaped(key) {
        if let Some(v) = value.filter(|_| name != key) {
            out.secrets.push((key.to_owned(), v.to_owned()));
        }
        return EnvVar::Secret;
    }
    match value {
        Some(v) => {
            out.notes
                .push(format!("env {key} inlined from the Hermes .env"));
            EnvVar::Plain(v.to_owned())
        }
        None => EnvVar::Dropped,
    }
}

fn environment(src: &Source<'_>, out: &mut Converted, value: &Value) {
    let Some(vars) = value.as_object() else {
        return;
    };
    let mut env = Map::new();
    let mut env_secrets = Vec::new();
    for (key, raw) in vars {
        let text = raw.as_str().map_or_else(|| raw.to_string(), str::to_owned);
        match env_var(src, out, key, text) {
            EnvVar::Plain(v) => {
                env.insert(key.clone(), json!(v));
            }
            EnvVar::Secret => env_secrets.push(key.clone()),
            EnvVar::Dropped => {}
        }
    }
    if !env.is_empty() {
        out.table.insert("env".to_owned(), Value::Object(env));
    }
    if !env_secrets.is_empty() {
        out.notes.push(format!(
            "credential environment moved to secrets: {}",
            env_secrets.join(", ")
        ));
        out.table
            .insert("env_secrets".to_owned(), json!(env_secrets));
    }
}

fn credential_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization"
        || ["token", "key", "secret", "password", "cookie"]
            .iter()
            .any(|w| lower.contains(w))
}

/// One header value: a `${NAME}` reference kept as a secrets-store
/// placeholder, a literal credential moved to a derived secret, or a
/// plain value.
fn header(src: &Source<'_>, server: &str, out: &mut Converted, name: &str, text: String) -> String {
    if text.contains("${") {
        let missing = refs::find(&text).into_iter().filter_map(|r| match r {
            refs::Ref::Env(var) if src.env_value(&var).is_none() => Some(var),
            _ => None,
        });
        for var in missing {
            out.notes.push(format!(
                "header {name}: ${{{var}}} is not set in the Hermes .env"
            ));
        }
        out.notes
            .push(format!("header {name} references the secrets store"));
        return refs::as_placeholders(&text);
    }
    if !credential_header(name) {
        return text;
    }
    let secret = derived(server, name);
    let (prefix, token) = match text.split_once(' ') {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => {
            (format!("{scheme} "), token.to_owned())
        }
        _ => (String::new(), text),
    };
    out.secrets.push((secret.clone(), token));
    out.notes
        .push(format!("header {name} moved to secret {secret}"));
    format!("{prefix}${{{secret}}}")
}

fn headers(src: &Source<'_>, server: &str, out: &mut Converted, value: &Value) {
    let Some(fields) = value.as_object() else {
        return;
    };
    let mut headers = Map::new();
    for (name, raw) in fields {
        let text = raw.as_str().map_or_else(|| raw.to_string(), str::to_owned);
        let kept = header(src, server, out, name, text);
        headers.insert(name.clone(), json!(kept));
    }
    out.table
        .insert("headers".to_owned(), Value::Object(headers));
}
