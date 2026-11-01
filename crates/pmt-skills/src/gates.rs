//! Which skills a session may see (spec: skills, Platform, environment and
//! conditional gates; Disabled skills).
//!
//! Three layers, from strongest:
//! - *disabled* (`skills.disabled` plus `skills.platform_disabled.<surface>`)
//!   and a *platform* mismatch hide a skill everywhere: index, listings,
//!   slash commands; `skill_view` explains why;
//! - *conditions* (`requires_*`, `fallback_for_*`, `session_platforms`)
//!   only keep a skill out of the prompt index: the model can still list
//!   and load it when it needs to.

use std::collections::BTreeSet;

use pmt_config::schema::SkillsConfig;

use crate::catalog::Skill;

/// The host OS as skills name it.
#[must_use]
pub const fn host_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// What a session offers, for the gates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionView {
    /// The surface or adapter (`cli`, `telegram`...); `None` = unknown.
    pub platform: Option<String>,
    /// Exposed tool names.
    pub tools: BTreeSet<String>,
    /// Selected toolsets (expanded).
    pub toolsets: BTreeSet<String>,
}

/// Why a skill is hidden from everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    /// Disabled by configuration.
    Disabled,
    /// Not for this OS.
    Platform,
}

impl Hidden {
    /// `skill_view`'s refusal for skill `name`.
    #[must_use]
    pub fn message(self, name: &str) -> String {
        match self {
            Self::Disabled => format!(
                "Skill '{name}' is disabled. Enable it with prometheus skills enable {name}."
            ),
            Self::Platform => format!("Skill '{name}' is not supported on this platform."),
        }
    }
}

/// Whether `name` is disabled for `platform`.
#[must_use]
pub fn disabled(config: &SkillsConfig, platform: Option<&str>, name: &str) -> bool {
    config.disabled.iter().any(|n| n == name)
        || platform
            .and_then(|p| config.platform_disabled.get(p))
            .is_some_and(|names| names.iter().any(|n| n == name))
}

/// Whether `skill` runs on `host` (an empty `platforms` list = all).
#[must_use]
pub fn platform_ok(skill: &Skill, host: &str) -> bool {
    skill.meta.platforms.is_empty() || skill.meta.platforms.iter().any(|p| p == host)
}

/// Why `skill` is hidden everywhere in a session, if it is.
#[must_use]
pub fn hidden(config: &SkillsConfig, view: &SessionView, skill: &Skill) -> Option<Hidden> {
    if disabled(config, view.platform.as_deref(), &skill.name) {
        Some(Hidden::Disabled)
    } else if !platform_ok(skill, host_platform()) {
        Some(Hidden::Platform)
    } else {
        None
    }
}

/// Whether the conditional gates let `skill` into the prompt index.
#[must_use]
pub fn conditions_ok(view: &SessionView, skill: &Skill) -> bool {
    let meta = &skill.meta;
    let has_tool = |t: &String| view.tools.contains(t);
    let has_toolset = |t: &String| view.toolsets.contains(t);
    let surface_ok = match (&meta.session_platforms, &view.platform) {
        (Some(allowed), Some(platform)) => allowed.iter().any(|p| p == platform),
        _ => true,
    };
    meta.requires_tools.iter().all(has_tool)
        && meta.requires_toolsets.iter().all(has_toolset)
        && !meta.fallback_for_tools.iter().any(has_tool)
        && !meta.fallback_for_toolsets.iter().any(has_toolset)
        && surface_ok
}
