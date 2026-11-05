//! MCP entries of the plugin catalog (spec: plugin-system, MCP catalog
//! entries).
//!
//! The signed catalog document also carries an `mcp` list. An entry names
//! how the server is reached (`transport`, `url` or `command` and `args`,
//! `auth`), optionally a git `source` pinned to a full commit SHA with
//! `bootstrap` commands run in the sandbox after checkout, the `setup`
//! fields an install asks for (secret ones are masked and go to the
//! secrets store, referenced as `${NAME}`), which tools start enabled
//! (`tools.default_enabled` or `tools.default_excluded`), `post_install`
//! notes and advisory `suggest` metadata. An entry with a newer
//! `manifest_version` than this build reads is kept (shown with a
//! warning) but cannot be installed. Invalid entries are skipped with a
//! WARN, like plugin entries.

use serde::Deserialize;

/// The newest MCP entry manifest version this build installs.
pub const MCP_MANIFEST_VERSION: u32 = 1;
/// `suggest.applications` holds at most this many.
pub const MAX_APPLICATIONS: usize = 16;
/// `suggest.examples` holds at most this many.
pub const MAX_EXAMPLES: usize = 6;

/// A git checkout the server runs from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The repository (`https://`, or `file://` for a local catalog).
    pub repo: String,
    /// A full 40-hex commit SHA.
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// Commands run in the checkout, in the sandbox, after cloning.
    #[serde(default)]
    pub bootstrap: Vec<String>,
}

/// One value an install asks for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupField {
    /// Its name (`${NAME}` in the entry); a secret's secrets-store name.
    pub name: String,
    /// The question.
    pub prompt: String,
    /// Masked input, kept in the secrets store.
    #[serde(default)]
    pub secret: bool,
    /// The answer when none is given.
    #[serde(default)]
    pub default: Option<String>,
}

/// Which tools start enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefaults {
    /// Only these (exact names or globs).
    #[serde(default)]
    pub default_enabled: Option<Vec<String>>,
    /// All but these: a large surface installs without a checklist.
    #[serde(default)]
    pub default_excluded: Option<Vec<String>>,
}

/// When the entry may be suggested (advisory only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suggest {
    /// Words that hint at it.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Hosts whose URLs hint at it.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Applications it works with (at most 16).
    #[serde(default)]
    pub applications: Vec<String>,
    /// Example requests (at most 6).
    #[serde(default)]
    pub examples: Vec<String>,
    /// An application that must be installed.
    #[serde(default)]
    pub requires_app: Option<String>,
}

/// One MCP catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEntry {
    /// The entry id (the default server name).
    pub id: String,
    /// Its manifest version.
    pub manifest_version: u32,
    /// One line for listings.
    #[serde(default)]
    pub description: String,
    /// `stdio` or `http`.
    pub transport: String,
    /// `none`, `header` or `oauth`.
    #[serde(default = "none")]
    pub auth: String,
    /// The endpoint of an `http` entry.
    #[serde(default)]
    pub url: String,
    /// The program of a `stdio` entry.
    #[serde(default)]
    pub command: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables of a `stdio` entry.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Headers of an `http` entry.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// A pinned git checkout.
    #[serde(default)]
    pub source: Option<Source>,
    /// What an install asks for.
    #[serde(default)]
    pub setup: Vec<SetupField>,
    /// Which tools start enabled.
    #[serde(default)]
    pub tools: ToolDefaults,
    /// Notes shown after an install.
    #[serde(default)]
    pub post_install: String,
    /// When it may be suggested.
    #[serde(default)]
    pub suggest: Suggest,
}

fn none() -> String {
    "none".to_owned()
}

impl McpEntry {
    /// Whether this build is too old to install it.
    #[must_use]
    pub fn needs_newer(&self) -> bool {
        self.manifest_version > MCP_MANIFEST_VERSION
    }

    /// Parses one entry, checking what the type cannot.
    ///
    /// # Errors
    ///
    /// Why the entry is invalid.
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let entry: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        entry.check()?;
        Ok(entry)
    }

    fn check(&self) -> Result<(), String> {
        let id = &self.id;
        let valid_id = !id.is_empty()
            && id.len() <= 64
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        if !valid_id {
            return Err(format!("mcp entry '{id}': invalid id"));
        }
        self.check_transport()
            .and_then(|()| self.check_source())
            .map_err(|why| format!("mcp entry '{id}': {why}"))?;
        if self.tools.default_enabled.is_some() && self.tools.default_excluded.is_some() {
            return Err(format!(
                "mcp entry '{id}': tools.default_enabled and tools.default_excluded exclude each other"
            ));
        }
        if self.suggest.applications.len() > MAX_APPLICATIONS
            || self.suggest.examples.len() > MAX_EXAMPLES
        {
            return Err(format!(
                "mcp entry '{id}': suggest holds at most {MAX_APPLICATIONS} applications and {MAX_EXAMPLES} examples"
            ));
        }
        Ok(())
    }

    fn check_transport(&self) -> Result<(), String> {
        let url = self.url.starts_with("https://") || self.url.starts_with("http://");
        match self.transport.as_str() {
            "http" if !url => return Err("an http entry needs a url".to_owned()),
            "stdio" if self.command.is_empty() => {
                return Err("a stdio entry needs a command".to_owned());
            }
            "http" | "stdio" => {}
            other => return Err(format!("unknown transport '{other}'")),
        }
        if matches!(self.auth.as_str(), "none" | "header" | "oauth") {
            Ok(())
        } else {
            Err(format!("unknown auth '{}'", self.auth))
        }
    }

    fn check_source(&self) -> Result<(), String> {
        let Some(source) = &self.source else {
            return Ok(());
        };
        if !crate::catalog::is_commit(&source.r#ref) {
            return Err("source.ref must be a full commit SHA".to_owned());
        }
        if !source.repo.starts_with("https://") && !source.repo.starts_with("file://") {
            return Err("source.repo must be https://".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entries_are_checked() {
        let good = json!({
            "id": "linear", "manifest_version": 1, "transport": "http",
            "url": "https://mcp.linear.app/mcp", "auth": "oauth",
            "tools": {"default_excluded": ["*_delete*"]},
        });
        let entry = McpEntry::parse(&good).unwrap();
        assert!(!entry.needs_newer());
        let mut newer = good.clone();
        newer["manifest_version"] = json!(2);
        assert!(McpEntry::parse(&newer).unwrap().needs_newer());
        let mut both = good.clone();
        both["tools"] = json!({"default_enabled": ["a"], "default_excluded": ["b"]});
        assert!(McpEntry::parse(&both).is_err());
        let symbolic = json!({
            "id": "x", "manifest_version": 1, "transport": "stdio", "command": "node",
            "source": {"repo": "https://github.com/x/y", "ref": "main"},
        });
        assert!(
            McpEntry::parse(&symbolic)
                .unwrap_err()
                .contains("full commit SHA")
        );
    }
}
