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
            owner: Owner::Builtin,
        }
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
    vec![
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
        Toolset::new(
            "browser",
            "Browser automation",
            &[
                "browser_exec",
                "browser_cdp",
                "browser_dialog",
                "browser_vault",
            ],
        ),
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
        coding,
    ]
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
