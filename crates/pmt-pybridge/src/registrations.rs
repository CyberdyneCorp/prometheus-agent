//! What a plugin registered in its sidecar (the `plugin.load` answer),
//! cached per plugin content so a later session knows it without starting
//! the sidecar (spec: python-bridge, Sidecar lifecycle; Registrations
//! follow plugin-system reload rules).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The tool name.
    pub name: String,
    /// The Hermes toolset it named.
    #[serde(default)]
    pub toolset: String,
    /// The OpenAI-style function schema (`name`, `description`, `parameters`).
    #[serde(default)]
    pub schema: Value,
    /// One line for the model.
    #[serde(default)]
    pub description: String,
    /// It replaces a core tool.
    #[serde(default)]
    pub r#override: bool,
}

impl Tool {
    /// The JSON-Schema of its arguments.
    #[must_use]
    pub fn parameters(&self) -> Value {
        match self.schema.get("parameters") {
            Some(p) if p.is_object() => p.clone(),
            _ => serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    /// The description, falling back to the schema's.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.description.is_empty() {
            self.schema["description"].as_str().unwrap_or("").to_owned()
        } else {
            self.description.clone()
        }
    }
}

/// One system-prompt section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// Its id.
    pub id: String,
    /// The size it allows itself.
    #[serde(default)]
    pub max_chars: u64,
    /// Its text when it is a constant (a callable renders per session).
    #[serde(default)]
    pub r#static: Option<String>,
}

/// One slash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Name without `/`.
    pub name: String,
    /// One line for `/help`.
    #[serde(default)]
    pub description: String,
    /// Its argument hint.
    #[serde(default)]
    pub args_hint: String,
}

/// One `prometheus plugins run` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliCommand {
    /// Its name.
    pub name: String,
    /// One line of help.
    #[serde(default)]
    pub help: String,
    /// Longer description.
    #[serde(default)]
    pub description: String,
}

/// Everything one load registered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registrations {
    /// Tools.
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// Hook names.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// System-prompt sections.
    #[serde(default)]
    pub sections: Vec<Section>,
    /// Slash commands.
    #[serde(default)]
    pub commands: Vec<Command>,
    /// CLI commands.
    #[serde(default)]
    pub cli_commands: Vec<CliCommand>,
    /// Model-provider profiles, as the shim describes them.
    #[serde(default)]
    pub providers: Vec<Value>,
    /// Provider objects by category (`web_search` -> names).
    #[serde(default)]
    pub provider_objects: std::collections::BTreeMap<String, Vec<String>>,
    /// Memory providers.
    #[serde(default)]
    pub memory_providers: Vec<String>,
    /// Context engines.
    #[serde(default)]
    pub context_engines: Vec<String>,
    /// Approval transports.
    #[serde(default)]
    pub transports: Vec<String>,
    /// Event subscriptions.
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// Skills.
    #[serde(default)]
    pub skills: Vec<Value>,
    /// Why the plugin runs degraded.
    #[serde(default)]
    pub degraded: Vec<String>,
}

impl Registrations {
    /// Whether the sidecar must run from the first session on: hooks,
    /// providers or sections whose text is computed (spec: Sidecar
    /// lifecycle).
    #[must_use]
    pub fn eager(&self) -> bool {
        !self.hooks.is_empty()
            || !self.providers.is_empty()
            || self.provider_objects.values().any(|v| !v.is_empty())
            || !self.memory_providers.is_empty()
            || !self.context_engines.is_empty()
            || !self.transports.is_empty()
            || !self.subscriptions.is_empty()
            || self.sections.iter().any(|s| s.r#static.is_none())
    }

    /// Whether it still provides tool `name`.
    #[must_use]
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }
}

/// `<profile>/cache/bridge/<plugin>-<key12>.json`.
#[must_use]
pub fn cache_path(profile_dir: &Path, plugin: &str, key: &str) -> PathBuf {
    profile_dir
        .join("cache")
        .join("bridge")
        .join(format!("{plugin}-{}.json", &key[..key.len().min(12)]))
}

/// The cached registrations, when present and readable.
#[must_use]
pub fn read_cache(path: &Path) -> Option<Registrations> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Caches `registrations` (best effort).
pub fn write_cache(path: &Path, registrations: &Registrations) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(registrations) {
        let _ = std::fs::write(path, bytes);
    }
}
