//! `[capabilities]`: what a plugin asks for (spec: plugin-system,
//! Capability declarations) and its mapping onto the capability vocabulary
//! that grants are stored in (security-boundaries, Typed capability
//! vocabulary).

use std::path::Path;

use pmt_core::capability::{Capability, CapabilitySet, PluginRight};
use pmt_core::error::Error;
use toml::{Table, Value};

use super::{invalid, strings};

/// Every key `[capabilities]` may hold.
const KEYS: &[&str] = &[
    "network",
    "fs_read",
    "fs_write",
    "secrets",
    "llm",
    "tools_call",
    "tools_override",
    "hooks",
    "prompt_section",
    "commands",
    "cli_commands",
    "providers",
    "providers_override",
    "approval_transport",
    "inject_messages",
    "platform_actions",
    "subprocess",
    "ui",
    "subagents",
];

/// The keys of the `llm` table.
const LLM_KEYS: &[&str] = &[
    "enabled",
    "allow_model_override",
    "allow_provider_override",
    "allowed_models",
];

/// The surface `ui = true` maps to (`ui.plugin_panel`).
const UI_SURFACE: &str = "plugin_panel";

/// The declared capabilities, as written (the flags mirror the manifest's
/// independent boolean keys).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Declared {
    /// `host[:port]` patterns (`*.` prefix allowed, no bare `*`).
    pub network: Vec<String>,
    /// Readable paths under `$plugin_data`, `$workspace` or absolute.
    pub fs_read: Vec<String>,
    /// Writable paths, as `fs_read`.
    pub fs_write: Vec<String>,
    /// Secret names.
    pub secrets: Vec<String>,
    /// Model access.
    pub llm: Option<LlmDeclaration>,
    /// Tools it may dispatch.
    pub tools_call: Vec<String>,
    /// Core tools it may replace.
    pub tools_override: Vec<String>,
    /// Hooks it subscribes to (closed list).
    pub hooks: Vec<String>,
    /// Largest system-prompt section, at most 4000 characters.
    pub prompt_section: Option<u64>,
    /// Slash commands.
    pub commands: Vec<String>,
    /// `prometheus plugins run <id>` commands.
    pub cli_commands: Vec<String>,
    /// Provider ids it registers.
    pub providers: Vec<String>,
    /// Built-in providers it replaces.
    pub providers_override: Vec<String>,
    /// Delivers approval prompts.
    pub approval_transport: bool,
    /// Injects messages into sessions.
    pub inject_messages: bool,
    /// `add_reaction`, `set_thread_title`.
    pub platform_actions: Vec<String>,
    /// Starts child processes.
    pub subprocess: bool,
    /// Shows a UI panel.
    pub ui: bool,
    /// Launches subagents (the `subagents` host interface).
    pub subagents: bool,
}

/// `llm = { enabled, allow_model_override, allow_provider_override, allowed_models }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmDeclaration {
    /// Calls the profile's active model.
    pub enabled: bool,
    /// May name a model in `allowed_models`.
    pub allow_model_override: bool,
    /// May name a provider.
    pub allow_provider_override: bool,
    /// Models it may name (`["*"]` = any).
    pub allowed_models: Vec<String>,
}

impl Declared {
    /// Parses `[capabilities]` (absent = nothing).
    ///
    /// # Errors
    ///
    /// `plugins.manifest`: `unknown capability '<key>'`, an unknown hook,
    /// a bare `*` network pattern, an oversized prompt section or a value
    /// of the wrong type.
    pub fn parse(value: Option<&Value>) -> Result<Self, Error> {
        let table = match value {
            None => return Ok(Self::default()),
            Some(Value::Table(table)) => table,
            Some(_) => return Err(invalid("capabilities must be a table".to_owned())),
        };
        if let Some(key) = table.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(invalid(format!("unknown capability '{key}'")));
        }
        let declared = Self {
            network: strings(table, "network")?,
            fs_read: strings(table, "fs_read")?,
            fs_write: strings(table, "fs_write")?,
            secrets: strings(table, "secrets")?,
            llm: llm(table.get("llm"))?,
            tools_call: strings(table, "tools_call")?,
            tools_override: strings(table, "tools_override")?,
            hooks: strings(table, "hooks")?,
            prompt_section: table
                .get("prompt_section")
                .map(|v| {
                    v.as_integer()
                        .and_then(|n| u64::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("prompt_section must be a number of characters".to_owned())
                        })
                })
                .transpose()?,
            commands: strings(table, "commands")?,
            cli_commands: strings(table, "cli_commands")?,
            providers: strings(table, "providers")?,
            providers_override: strings(table, "providers_override")?,
            approval_transport: flag(table, "approval_transport")?,
            inject_messages: flag(table, "inject_messages")?,
            platform_actions: strings(table, "platform_actions")?,
            subprocess: flag(table, "subprocess")?,
            ui: flag(table, "ui")?,
            subagents: flag(table, "subagents")?,
        };
        declared.check()?;
        Ok(declared)
    }

    fn check(&self) -> Result<(), Error> {
        for hook in &self.hooks {
            pmt_core::hooks::check(hook).map_err(|_| invalid(format!("unknown hook '{hook}'")))?;
        }
        if self.network.iter().any(|p| p == "*") {
            return Err(invalid(
                "network: a bare '*' is not allowed; list the hosts (`*.example.com` is)"
                    .to_owned(),
            ));
        }
        if let Some(size) = self.prompt_section
            && !(1..=pmt_core::capability::MAX_PROMPT_SECTION_CHARS).contains(&size)
        {
            return Err(invalid(format!(
                "prompt_section must be 1 to 4000 characters, not {size}"
            )));
        }
        // Every value must map onto the vocabulary (bad hosts, globs,
        // secret names, verbs).
        self.requested(Path::new("/plugin-data")).map(|_| ())
    }

    /// The capabilities this declaration asks for; `$plugin_data` paths
    /// resolve under `plugin_data` (the instance profile's
    /// `plugin-data/<id>/`) and `$workspace` to the session workspace.
    ///
    /// # Errors
    ///
    /// `plugins.manifest` naming a value the vocabulary refuses.
    pub fn requested(&self, plugin_data: &Path) -> Result<CapabilitySet, Error> {
        let mut texts: Vec<String> = Vec::new();
        texts.extend(self.network.iter().map(|h| format!("net.connect:{h}")));
        for (key, paths) in [("fs.read", &self.fs_read), ("fs.write", &self.fs_write)] {
            for path in paths {
                texts.push(format!("{key}:{}", place(path, plugin_data)?));
            }
        }
        texts.extend(self.secrets.iter().map(|s| format!("secrets.read:{s}")));
        if let Some(llm) = &self.llm {
            texts.extend(llm.capabilities());
        }
        texts.extend(self.tools_call.iter().map(|t| format!("tools.call:{t}")));
        texts.extend(
            self.tools_override
                .iter()
                .map(|t| format!("tools.override:{t}")),
        );
        let rights: [(&str, &Vec<String>); 6] = [
            ("hook", &self.hooks),
            ("command", &self.commands),
            ("cli_command", &self.cli_commands),
            ("provider", &self.providers),
            ("provider_override", &self.providers_override),
            ("platform_action", &self.platform_actions),
        ];
        for (right, values) in rights {
            texts.extend(values.iter().map(|v| format!("plugin.{right}:{v}")));
        }
        if let Some(size) = self.prompt_section {
            texts.push(format!("plugin.prompt_section:{size}"));
        }
        for (on, text) in [
            (self.approval_transport, "plugin.approval_transport"),
            (self.inject_messages, "plugin.inject_messages"),
            (self.subprocess, "process.spawn:*"),
            (self.subagents, "plugin.subagents"),
        ] {
            if on {
                texts.push(text.to_owned());
            }
        }
        if self.ui {
            texts.push(format!("ui.{UI_SURFACE}"));
        }
        texts
            .iter()
            .map(|t| {
                Capability::parse(t)
                    .map_err(|_| invalid(format!("invalid capability value: {}", label_text(t))))
            })
            .collect()
    }
}

impl LlmDeclaration {
    fn capabilities(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut out = vec!["llm.call".to_owned()];
        if self.allow_model_override {
            out.extend(
                self.allowed_models
                    .iter()
                    .map(|m| format!("plugin.llm_model:{m}")),
            );
        }
        if self.allow_provider_override {
            out.push("plugin.llm_provider_override".to_owned());
        }
        out
    }
}

fn llm(value: Option<&Value>) -> Result<Option<LlmDeclaration>, Error> {
    let table: &Table = match value {
        None => return Ok(None),
        Some(Value::Table(t)) => t,
        Some(_) => return Err(invalid("llm must be a table".to_owned())),
    };
    if let Some(key) = table.keys().find(|k| !LLM_KEYS.contains(&k.as_str())) {
        return Err(invalid(format!("unknown capability 'llm.{key}'")));
    }
    Ok(Some(LlmDeclaration {
        enabled: flag(table, "enabled")?,
        allow_model_override: flag(table, "allow_model_override")?,
        allow_provider_override: flag(table, "allow_provider_override")?,
        allowed_models: strings(table, "allowed_models")?,
    }))
}

fn flag(table: &Table, key: &str) -> Result<bool, Error> {
    match table.get(key) {
        None => Ok(false),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(_) => Err(invalid(format!("{key} must be true or false"))),
    }
}

/// `$plugin_data/x` -> `<plugin data>/x`, `$workspace/x` -> `<workspace>/x`.
fn place(path: &str, plugin_data: &Path) -> Result<String, Error> {
    if let Some(rest) = path.strip_prefix("$plugin_data") {
        let base = plugin_data.to_string_lossy().replace('\\', "/");
        return Ok(format!("{base}{rest}"));
    }
    if let Some(rest) = path.strip_prefix("$workspace") {
        return Ok(format!("<workspace>{rest}"));
    }
    if Path::new(path).is_absolute() {
        return Ok(path.to_owned());
    }
    Err(invalid(format!(
        "fs path '{path}' must start with $plugin_data, $workspace or be absolute"
    )))
}

/// A capability in the manifest's words, as approval prompts and
/// `capability_denied` messages say it: `network api.example.com`,
/// `secrets GITHUB_TOKEN`, `hooks pre_tool_call`.
#[must_use]
pub fn label(capability: &Capability) -> String {
    label_text(&capability.to_string())
}

fn label_text(text: &str) -> String {
    let (name, target) = text.split_once(':').unwrap_or((text, ""));
    let key = match name {
        "net.connect" => "network",
        "fs.read" => "fs_read",
        "fs.write" => "fs_write",
        "secrets.read" => "secrets",
        "llm.call" => "llm",
        "tools.call" => "tools_call",
        "tools.override" => "tools_override",
        "process.spawn" => return "subprocess".to_owned(),
        "plugin.hook" => "hooks",
        "plugin.command" => "commands",
        "plugin.cli_command" => "cli_commands",
        "plugin.provider" => "providers",
        "plugin.provider_override" => "providers_override",
        "plugin.platform_action" => "platform_actions",
        "plugin.prompt_section" => "prompt_section",
        "plugin.llm_model" => "llm model override",
        "plugin.llm_provider_override" => "llm provider override",
        other => other.strip_prefix("plugin.").unwrap_or(other),
    };
    if target.is_empty() {
        key.to_owned()
    } else {
        format!("{key} {target}")
    }
}

/// One capability in plain language, for the approval screen.
#[must_use]
pub fn describe(capability: &Capability) -> String {
    match capability {
        Capability::NetConnect { .. } => {
            let target = capability.to_string();
            let host = target.trim_start_matches("net.connect:");
            format!("connect to {host}")
        }
        Capability::FsRead(glob) => format!("read files in {glob}"),
        Capability::FsWrite(glob) => format!("write files in {glob}"),
        Capability::SecretsRead(name) => format!("read the secret {name}"),
        Capability::LlmCall(_) => "call the profile's model (charged to the session)".to_owned(),
        Capability::ToolsCall(tool) => format!("call the tool {tool}"),
        Capability::ToolsOverride(tool) => format!("replace the built-in tool {tool}"),
        Capability::ProcessSpawn(_) => "start programs".to_owned(),
        Capability::Ui(_) => "show a panel in the dashboard and desktop app".to_owned(),
        Capability::Plugin { right, target } => describe_right(*right, target.as_deref()),
        other => other.to_string(),
    }
}

fn describe_right(right: PluginRight, target: Option<&str>) -> String {
    let target = target.unwrap_or_default();
    match right {
        PluginRight::Hook => format!("run on the {target} hook"),
        PluginRight::Command => format!("add the slash command /{target}"),
        PluginRight::CliCommand => {
            format!("add the command `prometheus plugins run <id> {target}`")
        }
        PluginRight::PromptSection => {
            format!("add up to {target} characters to the system prompt")
        }
        PluginRight::Provider => format!("register the provider {target}"),
        PluginRight::ProviderOverride => format!("replace the built-in provider {target}"),
        PluginRight::ApprovalTransport => "deliver approval prompts".to_owned(),
        PluginRight::InjectMessages => "inject messages into sessions".to_owned(),
        PluginRight::PlatformAction => {
            format!("{} on messaging platforms", target.replace('_', " "))
        }
        PluginRight::LlmModel if target == "*" => "choose any model".to_owned(),
        PluginRight::LlmModel => format!("choose the model {target}"),
        PluginRight::LlmProviderOverride => "choose the provider".to_owned(),
        PluginRight::Subagents => "launch subagents in the calling session".to_owned(),
    }
}
