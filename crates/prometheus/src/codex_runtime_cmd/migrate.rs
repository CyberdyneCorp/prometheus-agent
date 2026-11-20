//! The Codex runtime migration (spec: model-providers, Codex runtime
//! migration): a delimited managed section of `~/.codex/config.toml`
//! carrying the profile's MCP servers, the Codex plugins to enable and
//! the Prometheus callback server. Everything outside the section is the
//! user's and stays byte for byte; the section is replaced in place, so
//! running the migration again changes nothing else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_config::schema::{McpServerConfig, McpTransport};
use serde::Serialize;
use toml::{Table, Value};

/// The section's first line.
pub const BEGIN: &str =
    "# >>> prometheus managed section (rewritten by `prometheus codex-runtime migrate`) >>>";
/// The section's last line.
pub const END: &str = "# <<< prometheus managed section <<<";
/// The callback server's name in `[mcp_servers]`.
pub const CALLBACK: &str = "prometheus";
/// The tools the callback server exposes to Codex.
pub const CALLBACK_TOOLS: [&str; 7] = [
    "web_search",
    "web_extract",
    "vision_analyze",
    "image_generate",
    "skills_list",
    "skill_view",
    "text_to_speech",
];

/// What a migration did (`--json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Profile MCP servers written into the section.
    pub migrated: Vec<String>,
    /// User-authored `[mcp_servers]` entries left as they were.
    pub preserved_user_servers: Vec<String>,
    /// Keys Codex has no equivalent for, per server (dropped).
    pub skipped_keys_per_server: BTreeMap<String, Vec<String>>,
    /// What could not be migrated.
    pub errors: Vec<String>,
    /// The Codex config file.
    pub target_path: String,
    /// Whether the file was written (`false` with `--dry-run`).
    pub written: bool,
}

/// What goes into the section.
pub struct Inputs<'a> {
    /// The profile's `mcp.servers`.
    pub servers: &'a BTreeMap<String, McpServerConfig>,
    /// Codex plugins to enable (installed and available).
    pub plugins: &'a [String],
    /// The callback server's command and arguments.
    pub callback: (String, Vec<String>),
}

/// The new text of a config whose current text is `existing`, and the
/// report (`written` false).
///
/// # Errors
///
/// The text outside the section is not valid TOML (nothing is written).
pub fn plan(
    existing: &str,
    inputs: &Inputs<'_>,
    target: &Path,
) -> Result<(String, Report), String> {
    let (before, after) = split(existing);
    let user: Table = format!("{before}\n{after}").parse().map_err(|e| {
        format!(
            "{} is not valid TOML outside the managed section: {e}",
            target.display()
        )
    })?;
    let taken = |table: &str| -> Vec<String> {
        user.get(table)
            .and_then(Value::as_table)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    };
    let user_servers = taken("mcp_servers");
    let user_plugins = taken("plugins");
    let mut report = Report {
        preserved_user_servers: user_servers.clone(),
        target_path: target.display().to_string(),
        ..Report::default()
    };
    let mut servers = Table::new();
    for (name, server) in inputs.servers {
        if user_servers.contains(name) || name == CALLBACK {
            report.errors.push(format!(
                "mcp server '{name}' is already defined outside the managed section; kept that one"
            ));
            continue;
        }
        let (table, skipped) = translate(server);
        if !skipped.is_empty() {
            report.skipped_keys_per_server.insert(name.clone(), skipped);
        }
        servers.insert(name.clone(), Value::Table(table));
        report.migrated.push(name.clone());
    }
    if !user_servers.iter().any(|s| s == CALLBACK) {
        servers.insert(
            CALLBACK.to_owned(),
            Value::Table(callback(&inputs.callback)),
        );
    }
    let mut plugins = Table::new();
    for plugin in inputs.plugins.iter().filter(|p| !user_plugins.contains(p)) {
        let mut entry = Table::new();
        entry.insert("enabled".to_owned(), Value::Boolean(true));
        plugins.insert(plugin.clone(), Value::Table(entry));
    }
    let mut managed = Table::new();
    managed.insert("mcp_servers".to_owned(), Value::Table(servers));
    if !plugins.is_empty() {
        managed.insert("plugins".to_owned(), Value::Table(plugins));
    }
    let body = toml::to_string(&managed).map_err(|e| e.to_string())?;
    let section = format!("{BEGIN}\n{body}{END}\n");
    let text = join(before, &section, after);
    text.parse::<Table>()
        .map_err(|e| format!("the migrated config would not be valid TOML: {e}"))?;
    Ok((text, report))
}

/// The text before and after the managed section (the section and its
/// delimiters removed; all of `text` before when it has none).
fn split(text: &str) -> (&str, &str) {
    let Some(start) = text.find(BEGIN) else {
        return (text, "");
    };
    let rest = &text[start..];
    let end = rest.find(END).map_or(text.len(), |i| start + i + END.len());
    let after = text[end..].strip_prefix('\n').unwrap_or(&text[end..]);
    (&text[..start], after)
}

/// `before`, the section, `after`; a section new to the file goes at its
/// end after a blank line.
fn join(before: &str, section: &str, after: &str) -> String {
    let mut text = before.to_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if after.is_empty() && !text.is_empty() && !text.ends_with("\n\n") {
        text.push('\n');
    }
    text.push_str(section);
    text.push_str(after);
    text
}

/// A profile server as a Codex `[mcp_servers.<name>]` table, and the keys
/// it had that Codex cannot take.
fn translate(server: &McpServerConfig) -> (Table, Vec<String>) {
    let mut table = Table::new();
    let strings =
        |items: &[String]| Value::Array(items.iter().cloned().map(Value::String).collect());
    match server.transport {
        McpTransport::Stdio => {
            table.insert("command".to_owned(), Value::String(server.command.clone()));
            if !server.args.is_empty() {
                table.insert("args".to_owned(), strings(&server.args));
            }
            if !server.env.is_empty() {
                let env = server
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())));
                table.insert("env".to_owned(), Value::Table(env.collect()));
            }
            // Secrets stay out of the file: Codex forwards the variables
            // of these names from its own environment.
            if !server.env_secrets.is_empty() {
                table.insert("env_vars".to_owned(), strings(&server.env_secrets));
            }
        }
        McpTransport::Http => http(server, &mut table),
    }
    if let Some(secs) = server.timeout_secs {
        table.insert("tool_timeout_sec".to_owned(), Value::Integer(seconds(secs)));
    }
    if let Some(secs) = server.connect_timeout_secs {
        table.insert(
            "startup_timeout_sec".to_owned(),
            Value::Integer(seconds(secs)),
        );
    }
    if !server.enabled {
        table.insert("enabled".to_owned(), Value::Boolean(false));
    }
    if let Some(include) = &server.tools.include {
        table.insert("enabled_tools".to_owned(), strings(include));
    }
    if !server.tools.exclude.is_empty() {
        table.insert("disabled_tools".to_owned(), strings(&server.tools.exclude));
    }
    (table, skipped(server))
}

fn seconds(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// The streamable-HTTP keys: the bearer secret and `${NAME}` header
/// placeholders become environment references, literal headers stay.
fn http(server: &McpServerConfig, table: &mut Table) {
    table.insert("url".to_owned(), Value::String(server.url.clone()));
    if !server.auth_secret.is_empty() {
        table.insert(
            "bearer_token_env_var".to_owned(),
            Value::String(server.auth_secret.clone()),
        );
    }
    let mut literal = Table::new();
    let mut from_env = Table::new();
    for (name, value) in &server.headers {
        match placeholder(value) {
            Some(var) => from_env.insert(name.clone(), Value::String(var.to_owned())),
            None if !value.contains("${") => {
                literal.insert(name.clone(), Value::String(value.clone()))
            }
            // A secret inside other text has no reference form: dropped.
            None => None,
        };
    }
    if !literal.is_empty() {
        table.insert("http_headers".to_owned(), Value::Table(literal));
    }
    if !from_env.is_empty() {
        table.insert("env_http_headers".to_owned(), Value::Table(from_env));
    }
}

/// `NAME` of a value that is exactly `${NAME}`.
fn placeholder(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")?
        .strip_suffix('}')
        .filter(|name| !name.is_empty() && !name.contains(['$', '{', '}']))
}

/// The keys `server` sets (differs from the default) that Codex has no
/// equivalent for.
fn skipped(server: &McpServerConfig) -> Vec<String> {
    const CARRIED: &[&str] = &[
        "enabled",
        "transport",
        "command",
        "args",
        "env",
        "env_secrets",
        "url",
        "auth_secret",
        "timeout_secs",
        "connect_timeout_secs",
        "tools",
    ];
    let (Ok(serde_json::Value::Object(set)), Ok(serde_json::Value::Object(default))) = (
        serde_json::to_value(server),
        serde_json::to_value(McpServerConfig::default()),
    ) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = set
        .iter()
        .filter(|(key, value)| !CARRIED.contains(&key.as_str()) && default.get(*key) != Some(value))
        .map(|(key, _)| key.clone())
        .collect();
    let dropped_headers: Vec<&String> = server
        .headers
        .iter()
        .filter(|(_, v)| v.contains("${") && placeholder(v).is_none())
        .map(|(k, _)| k)
        .collect();
    keys.retain(|k| k != "headers");
    keys.extend(dropped_headers.into_iter().map(|h| format!("headers.{h}")));
    keys
}

fn callback((program, args): &(String, Vec<String>)) -> Table {
    let mut table = Table::new();
    table.insert("command".to_owned(), Value::String(program.clone()));
    table.insert(
        "args".to_owned(),
        Value::Array(args.iter().cloned().map(Value::String).collect()),
    );
    table
}

/// Writes `text` to `target` (creating its directory), replacing it
/// atomically.
///
/// # Errors
///
/// I/O errors, as text.
pub fn write(target: &Path, text: &str) -> Result<(), String> {
    let dir = target
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let temp = dir.join(".config.toml.prometheus-tmp");
    std::fs::write(&temp, text).map_err(|e| format!("{}: {e}", temp.display()))?;
    std::fs::rename(&temp, target).map_err(|e| format!("{}: {e}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(servers: &BTreeMap<String, McpServerConfig>) -> Inputs<'_> {
        Inputs {
            servers,
            plugins: &[],
            callback: ("/bin/prometheus".to_owned(), vec!["mcp".to_owned()]),
        }
    }

    fn server(text: &str) -> McpServerConfig {
        toml::from_str(text).unwrap()
    }

    /// Scenario: User text preserved.
    #[test]
    fn user_text_is_preserved_and_the_section_appears_once() {
        let user = "model = \"gpt-5\" # mine\n\n[mcp_servers.mine]\ncommand = \"my-server\"\nargs = [\"--x\"]\n";
        let mut servers = BTreeMap::new();
        servers.insert(
            "files".to_owned(),
            server("command = \"fs-mcp\"\nargs = [\"/srv\"]\n"),
        );
        let target = Path::new("/c/config.toml");
        let (once, report) = plan(user, &inputs(&servers), target).unwrap();
        let (twice, _) = plan(&once, &inputs(&servers), target).unwrap();
        assert_eq!(once, twice);
        assert!(twice.starts_with(user), "{twice}");
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert_eq!(twice.matches(END).count(), 1);
        assert_eq!(report.preserved_user_servers, ["mine"]);
        assert_eq!(report.migrated, ["files"]);
        let parsed: Table = twice.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["files"]["command"].as_str(),
            Some("fs-mcp")
        );
        assert!(parsed["mcp_servers"][CALLBACK]["command"].is_str());
    }

    #[test]
    fn user_text_after_the_section_stays_where_it_was() {
        let text = format!(
            "a = 1\n{BEGIN}\n[mcp_servers.old]\ncommand = \"x\"\n{END}\n[profile]\nb = 2\n"
        );
        let (new, _) = plan(&text, &inputs(&BTreeMap::new()), Path::new("c")).unwrap();
        assert!(new.starts_with("a = 1\n"), "{new}");
        assert!(
            new.ends_with(&format!("{END}\n[profile]\nb = 2\n")),
            "{new}"
        );
        assert!(!new.contains("old"), "{new}");
    }

    #[test]
    fn servers_translate_and_secrets_stay_references() {
        let stdio = server(
            "command = \"gh-mcp\"\nenv = { MODE = \"ro\" }\nenv_secrets = [\"GITHUB_TOKEN\"]\ntimeout_secs = 90\nconnect_timeout_secs = 20\nenabled = false\nlazy = true\n",
        );
        let (table, skipped) = translate(&stdio);
        assert_eq!(
            table["env_vars"].as_array().unwrap()[0].as_str(),
            Some("GITHUB_TOKEN")
        );
        assert_eq!(table["env"]["MODE"].as_str(), Some("ro"));
        assert_eq!(table["tool_timeout_sec"].as_integer(), Some(90));
        assert_eq!(table["startup_timeout_sec"].as_integer(), Some(20));
        assert_eq!(table["enabled"].as_bool(), Some(false));
        assert_eq!(skipped, ["lazy"]);
        let remote = server(
            "transport = \"http\"\nurl = \"https://mcp.example/x\"\nauth_secret = \"MCP_TOKEN\"\nheaders = { X-Team = \"ops\", X-Key = \"${TEAM_KEY}\", X-Mixed = \"k ${A}\" }\n",
        );
        let (table, skipped) = translate(&remote);
        assert_eq!(table["bearer_token_env_var"].as_str(), Some("MCP_TOKEN"));
        assert_eq!(table["http_headers"]["X-Team"].as_str(), Some("ops"));
        assert_eq!(
            table["env_http_headers"]["X-Key"].as_str(),
            Some("TEAM_KEY")
        );
        assert_eq!(skipped, ["headers.X-Mixed"]);
        assert!(!toml::to_string(&table).unwrap().contains("k ${A}"));
    }

    #[test]
    fn a_name_the_user_owns_is_not_overwritten() {
        let user = "[mcp_servers.files]\ncommand = \"theirs\"\n";
        let mut servers = BTreeMap::new();
        servers.insert("files".to_owned(), server("command = \"ours\"\n"));
        let (text, report) = plan(user, &inputs(&servers), Path::new("c")).unwrap();
        assert!(report.migrated.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(!text.contains("ours"), "{text}");
    }
}
