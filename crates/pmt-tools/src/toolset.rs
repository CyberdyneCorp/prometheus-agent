//! Toolsets: named groups of tools that may include other toolsets (spec:
//! tool-system, Built-in toolsets and composition; Core toolset
//! membership; Single-purpose toolsets and default-off toolsets; Composite
//! toolsets and the coding posture).
//!
//! A toolset's members are its listed tools plus every tool registered
//! under it, then the members of its includes, in that order. Include
//! cycles are load errors naming the cycle.

use std::collections::{BTreeMap, BTreeSet};

use pmt_core::error::{Error, codes};
use pmt_core::registry::Registry;

use crate::descriptor::Owner;

/// How a toolset is built on `core` (spec: tool-system, Platform
/// bundles; Disabling a bundle never strips core). Disabling a toolset
/// built on `core` removes only its members that are not in `core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composition {
    /// Its members are what it lists and includes.
    Plain,
    /// Built on `core` (`api_server`).
    OnCore,
    /// The bundle of this messaging platform: its members also include
    /// the platform's own toolsets registered when it is expanded.
    Platform(String),
    /// `gateway`: its members also include every platform bundle
    /// registered when it is expanded.
    Gateway,
}

/// One toolset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    /// Name used in `tools.enabled_toolsets`.
    pub name: String,
    /// One line for listings.
    pub description: String,
    /// Tools listed directly, in order.
    pub tools: Vec<String>,
    /// Toolsets whose members are added after the direct tools.
    pub includes: Vec<String>,
    /// Never part of a default; exposed only when named explicitly.
    pub default_off: bool,
    /// Selecting it also selects a prompt posture (`coding`).
    pub posture: bool,
    /// Folded in only from the session's surface; never part of `all`.
    pub surface_gated: bool,
    /// How it is built on `core`.
    pub composition: Composition,
    /// Who registered it.
    pub owner: Owner,
}

impl Toolset {
    /// A toolset listing `tools` and including nothing.
    #[must_use]
    pub fn new(name: &str, description: &str, tools: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            tools: tools.iter().map(|&t| t.to_owned()).collect(),
            includes: Vec::new(),
            default_off: false,
            posture: false,
            surface_gated: false,
            composition: Composition::Plain,
            owner: Owner::Builtin,
        }
    }

    /// The bundle `platform-<platform>`: `core` plus the platform's own
    /// toolsets ([`platform_toolsets`]); folded in only from that
    /// platform's sessions.
    #[must_use]
    pub fn bundle(platform: &str) -> Self {
        let mut set = Self::new(
            &bundle_name(platform),
            &format!("The core tools plus those of the {platform} platform"),
            &[],
        )
        .including(&["core"]);
        set.surface_gated = true;
        set.composition = Composition::Platform(platform.to_owned());
        set
    }

    /// Adds includes.
    #[must_use]
    pub fn including(mut self, toolsets: &[&str]) -> Self {
        self.includes = toolsets.iter().map(|&t| t.to_owned()).collect();
        self
    }

    /// Marks the toolset default-off.
    #[must_use]
    pub fn default_off(mut self) -> Self {
        self.default_off = true;
        self
    }

    /// Sets the owner.
    #[must_use]
    pub fn owned_by(mut self, owner: Owner) -> Self {
        self.owner = owner;
        self
    }
}

/// The `browser` toolset.
const BROWSER: &[&str] = &[
    "browser_exec",
    "browser_cdp",
    "browser_dialog",
    "browser_vault",
    "browser_close_profile",
];

/// The `core` toolset, in its specified order.
pub const CORE: &[&str] = &[
    "web_search",
    "web_extract",
    "terminal",
    "process",
    "read_file",
    "write_file",
    "patch",
    "search_files",
    "vision_analyze",
    "image_generate",
    "skills_list",
    "skill_view",
    "skill_manage",
    "browser_exec",
    "browser_cdp",
    "browser_dialog",
    "browser_vault",
    "browser_close_profile",
    "text_to_speech",
    "todo",
    "memory",
    "session_search",
    "clarify",
    "execute_code",
    "delegate_task",
    "cronjob",
    "computer_use",
    "manage_connections",
];

/// The prefix of platform bundle names.
pub const BUNDLE_PREFIX: &str = "platform-";

/// The union of every platform bundle.
pub const GATEWAY: &str = "gateway";

/// The bundle name of `platform`: `platform-<platform>`.
#[must_use]
pub fn bundle_name(platform: &str) -> String {
    format!("{BUNDLE_PREFIX}{platform}")
}

/// The toolsets a platform contributes to its bundle, when registered:
/// Feishu's document and drive toolsets, otherwise the toolset named
/// after the platform (`discord`, `yuanbao`, a plugin platform's own
/// toolset). `discord_admin` is never part of a bundle: it is named
/// explicitly.
#[must_use]
pub fn platform_toolsets(platform: &str) -> Vec<String> {
    match platform {
        "feishu" => vec!["feishu_doc".to_owned(), "feishu_drive".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// The first-party messaging platforms with a bundle. The webhook
/// platform has none: its sessions start from `webhook-safe`.
pub const BUNDLED_PLATFORMS: &[&str] = &[
    "telegram", "discord", "slack", "email", "matrix", "signal", "whatsapp",
];

/// `core` tools the `api_server` toolset leaves out (no one is there to
/// answer, hear or watch).
const NOT_API: &[&str] = &["text_to_speech", "clarify", "computer_use"];

/// Tools unattended ACP sessions leave out: shell, processes and code
/// execution.
const NOT_ACP_UNATTENDED: &[&str] = &["terminal", "process", "execute_code", "browser_exec"];

/// `core` tools the `coding` posture leaves out.
const NOT_CODING: &[&str] = &[
    "image_generate",
    "text_to_speech",
    "cronjob",
    "computer_use",
];

/// The toolsets shipped with Prometheus.
#[must_use]
pub fn builtin() -> Vec<Toolset> {
    let coding: Vec<&str> = CORE
        .iter()
        .copied()
        .filter(|t| !NOT_CODING.contains(t))
        .collect();
    let mut coding = Toolset::new(
        "coding",
        "Software work: core without media generation, speech, scheduling or computer use",
        &coding,
    );
    coding.posture = true;
    let mut sets = vec![
        Toolset::new("core", "The default tool set of every surface", CORE),
        Toolset::new(
            "web",
            "Web search and page extraction",
            &["web_search", "web_extract"],
        ),
        Toolset::new("search", "Web search only", &["web_search"]),
        Toolset::new("x_search", "Search posts on X", &["x_search"]).default_off(),
        Toolset::new("vision", "Image analysis", &["vision_analyze"]),
        Toolset::new("video", "Video analysis", &["video_analyze"]).default_off(),
        Toolset::new("image_gen", "Image generation", &["image_generate"]),
        Toolset::new("video_gen", "Video generation", &["video_generate"]).default_off(),
        Toolset::new("computer_use", "Desktop control", &["computer_use"]),
        Toolset::new(
            "terminal",
            "Shell commands and background processes",
            &["terminal", "process"],
        ),
        Toolset::new("process", "Background process management", &["process"]),
        Toolset::new(
            "file",
            "Read, write, patch and search files",
            &["read_file", "write_file", "patch", "search_files"],
        ),
        Toolset::new(
            "skills",
            "List, view and manage skills",
            &["skills_list", "skill_view", "skill_manage"],
        ),
        Toolset::new("browser", "Browser automation", BROWSER),
        Toolset::new("cronjob", "Scheduled jobs", &["cronjob"]),
        Toolset::new("tts", "Text to speech", &["text_to_speech"]),
        Toolset::new("todo", "The session task list", &["todo"]),
        Toolset::new("memory", "Long-term memory", &["memory"]),
        Toolset::new("context_engine", "Tools of the active context engine", &[]),
        Toolset::new(
            "session_search",
            "Search past sessions",
            &["session_search"],
        ),
        Toolset::new(
            "connections",
            "Manage connectors and MCP connections",
            &["manage_connections"],
        ),
        Toolset::new("clarify", "Ask the user a question", &["clarify"]),
        Toolset::new(
            "messaging",
            "Send messages and reactions to messaging chats (needs the gateway daemon)",
            &["send_message"],
        ),
        Toolset::new("code_execution", "Run code in a sandbox", &["execute_code"]),
        Toolset::new(
            "delegation",
            "Delegate work to subagents",
            &["delegate_task"],
        ),
        Toolset::new("kanban", "Kanban boards", &[]).default_off(),
        Toolset::new(
            "debugging",
            "Terminal, processes, web and files",
            &["terminal", "process"],
        )
        .including(&["web", "file"]),
        Toolset::new(
            "safe",
            "Web, vision and image generation; nothing that executes",
            &[],
        )
        .including(&["web", "vision", "image_gen"]),
        // The default of webhook-triggered runs (spec: messaging-gateway,
        // Webhook route delivery and safe default toolset).
        Toolset::new(
            "webhook-safe",
            "Read-only web and vision tools for webhook-triggered runs; nothing that executes or writes",
            &["web_search", "web_extract", "vision_analyze", "clarify"],
        ),
        coding,
    ];
    sets.extend(gui_toolsets());
    sets.extend(surface_defaults());
    sets
}

/// The GUI surfaces' toolsets (spec: desktop-app, Desktop UI toolset
/// contract; First-run onboarding chat; projects, Project tool for GUI
/// sessions): `project` (default on for desktop and tui only), and the
/// surface-gated `desktop_ui` and `onboarding`.
fn gui_toolsets() -> Vec<Toolset> {
    vec![
        Toolset::new(
            crate::agent_level_desktop::PROJECT,
            "Switch or create the session's project (on by default for desktop and tui)",
            &["project"],
        )
        .default_off(),
        surface_gated(Toolset::new(
            crate::agent_level_desktop::DESKTOP_UI,
            "The desktop window: reactions, terminal and preview panes, tours, tips, appearance",
            crate::agent_level_desktop::DESKTOP_UI_TOOLS,
        )),
        surface_gated(Toolset::new(
            crate::agent_level_desktop::ONBOARDING,
            "The first-run onboarding chat's setup cards",
            &["setup_beat"],
        )),
    ]
}

/// The defaults of surfaces other than the CLI's (spec: tool-system,
/// Per-surface default toolsets; Platform bundles): `acp`,
/// `acp_unattended`, `api_server`, `gateway` and the platform bundles.
fn surface_defaults() -> Vec<Toolset> {
    let acp_unattended: Vec<&str> = CORE
        .iter()
        .copied()
        .filter(|t| !NOT_CODING.contains(t) && !NOT_ACP_UNATTENDED.contains(t))
        .collect();
    let mut sets = vec![
        Toolset::new("acp", "The coding tools for ACP editor sessions", &[]).including(&["coding"]),
        Toolset::new(
            "acp_unattended",
            "ACP tools for clients that cannot answer prompts: no shell or code execution",
            &acp_unattended,
        ),
        api_server(),
        gateway(),
    ];
    sets.extend(BUNDLED_PLATFORMS.iter().map(|p| Toolset::bundle(p)));
    sets
}

/// `api_server`: `core` without the tools that need a person present.
fn api_server() -> Toolset {
    let tools: Vec<&str> = CORE
        .iter()
        .copied()
        .filter(|t| !NOT_API.contains(t))
        .collect();
    let mut set = Toolset::new(
        "api_server",
        "The core tools without speech, clarify and computer use, for API server callers",
        &tools,
    );
    set.composition = Composition::OnCore;
    set
}

/// `set` folded in only from a session's surface.
fn surface_gated(mut set: Toolset) -> Toolset {
    set.surface_gated = true;
    set
}

/// `gateway`: every platform bundle.
fn gateway() -> Toolset {
    let mut set = Toolset::new(
        GATEWAY,
        "Every messaging platform bundle: core plus each platform's tools",
        &[],
    );
    set.surface_gated = true;
    set.composition = Composition::Gateway;
    set
}

/// Every tool name the shipped toolsets refer to, each once, in toolset
/// order (`core` first). This is the vocabulary documentation and prompts
/// may name; the generated tools reference lists it and `cargo xtask gen`
/// refuses references to any other name.
#[must_use]
pub fn builtin_tool_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for toolset in builtin() {
        for tool in toolset.tools {
            if !names.contains(&tool) {
                names.push(tool);
            }
        }
    }
    names
}

/// The toolsets known to one registry, keyed by name.
pub type Toolsets = BTreeMap<String, Toolset>;

/// `unknown toolset '<name>' (did you mean ...?)` (exit 78).
pub(crate) fn unknown(name: &str, known: impl Iterator<Item = String>) -> Error {
    let names = Registry::from_entries("toolset", known.map(|n| (n, ())));
    match names {
        Ok(names) => names.get(name).err().unwrap_or_else(|| {
            Error::new(
                &codes::REGISTRY_UNKNOWN_NAME,
                format!("unknown toolset '{name}'"),
            )
        }),
        Err(err) => err,
    }
}

/// Checks `batch` against the toolsets already `known`: every include
/// names a toolset and no include cycle exists.
///
/// # Errors
///
/// `registry.unknown_name` for an unknown include; `toolset.cycle` with
/// `toolset include cycle: a -> b -> a`.
pub fn check_batch(known: &Toolsets, batch: &[Toolset]) -> Result<(), Error> {
    let mut all: BTreeMap<&str, &Toolset> = known.iter().map(|(k, v)| (k.as_str(), v)).collect();
    for toolset in batch {
        all.insert(&toolset.name, toolset);
    }
    for toolset in batch {
        for include in &toolset.includes {
            if !all.contains_key(include.as_str()) {
                let names = all.keys().map(|&n| n.to_owned());
                let err = unknown(include, names);
                return Err(Error::new(
                    err.code(),
                    format!("toolset '{}': {}", toolset.name, err.message()),
                ));
            }
        }
    }
    let mut done = BTreeSet::new();
    let mut starts: Vec<&str> = batch.iter().map(|t| t.name.as_str()).collect();
    starts.sort_unstable();
    for start in starts {
        let mut path = Vec::new();
        if let Some(cycle) = find_cycle(&all, start, &mut path, &mut done) {
            return Err(Error::new(
                &codes::TOOLSET_CYCLE,
                format!("toolset include cycle: {}", cycle.join(" -> ")),
            ));
        }
    }
    Ok(())
}

/// Depth-first search; returns the cycle (closing name repeated) when one
/// is reachable from `name`.
fn find_cycle<'a>(
    all: &BTreeMap<&'a str, &'a Toolset>,
    name: &'a str,
    path: &mut Vec<&'a str>,
    done: &mut BTreeSet<&'a str>,
) -> Option<Vec<&'a str>> {
    if let Some(at) = path.iter().position(|&n| n == name) {
        let mut cycle = path[at..].to_vec();
        cycle.push(name);
        return Some(cycle);
    }
    if done.contains(name) {
        return None;
    }
    path.push(name);
    let includes = all.get(name).map_or(&[][..], |t| t.includes.as_slice());
    for include in includes {
        if let Some(cycle) = find_cycle(all, include, path, done) {
            return Some(cycle);
        }
    }
    path.pop();
    done.insert(name);
    None
}

#[cfg(test)]
mod tests {
    use super::{Toolset, Toolsets, builtin, check_batch};
    use crate::descriptor::Owner;

    fn known() -> Toolsets {
        builtin().into_iter().map(|t| (t.name.clone(), t)).collect()
    }

    fn plugin(name: &str, includes: &[&str]) -> Toolset {
        Toolset::new(name, "", &[])
            .including(includes)
            .owned_by(Owner::Plugin("p".into()))
    }

    #[test]
    fn builtin_toolsets_load_without_cycles() {
        assert!(check_batch(&Toolsets::new(), &builtin()).is_ok());
    }

    #[test]
    fn include_cycle_is_reported_with_its_path() {
        let err = check_batch(&known(), &[plugin("a", &["b"]), plugin("b", &["a"])]).unwrap_err();
        assert_eq!(err.code().code, "toolset.cycle");
        assert_eq!(err.message(), "toolset include cycle: a -> b -> a");
        assert_eq!(err.exit_code(), 78);

        let err = check_batch(
            &known(),
            &[
                plugin("x", &["web", "y"]),
                plugin("y", &["z"]),
                plugin("z", &["y"]),
            ],
        )
        .unwrap_err();
        assert_eq!(err.message(), "toolset include cycle: y -> z -> y");

        let err = check_batch(&known(), &[plugin("self", &["self"])]).unwrap_err();
        assert_eq!(err.message(), "toolset include cycle: self -> self");
    }

    #[test]
    fn unknown_include_is_named() {
        let err = check_batch(&known(), &[plugin("a", &["fiel"])]).unwrap_err();
        assert_eq!(
            err.message(),
            "toolset 'a': unknown toolset 'fiel' (did you mean 'file'?)"
        );
    }
}
