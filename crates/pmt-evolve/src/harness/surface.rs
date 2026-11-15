//! The editable surface and its component numbering (spec:
//! harness-evolution, Editable surface and component numbering).
//!
//! Only these paths may be in a harness; everything else (user memory,
//! `config.toml`, secrets, installed skills) is off the surface, so no
//! proposal can touch it.

use std::sync::LazyLock;

use regex::Regex;

/// One numbered component of the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component {
    /// c1 `SYSTEM.md`: the system prompt overlay.
    System,
    /// c2 `GUARDRAILS.md`.
    Guardrails,
    /// c3 `MEMORY_POLICY.md`.
    MemoryPolicy,
    /// c4 `TOOL_GUIDANCE.md`.
    ToolGuidance,
    /// c5 `agents/<name>/AGENT.md`: subagent definitions.
    Agents,
    /// c6 `skills/<name>/SKILL.md` and `skills/<name>/scripts/<file>`.
    Skills,
    /// c7 `scripts/<file>`.
    Scripts,
    /// c8 `hooks.toml`.
    Hooks,
}

impl Component {
    /// Every component, in number order.
    pub const ALL: [Self; 8] = [
        Self::System,
        Self::Guardrails,
        Self::MemoryPolicy,
        Self::ToolGuidance,
        Self::Agents,
        Self::Skills,
        Self::Scripts,
        Self::Hooks,
    ];

    /// `c1`..`c8`.
    #[must_use]
    pub const fn number(self) -> &'static str {
        match self {
            Self::System => "c1",
            Self::Guardrails => "c2",
            Self::MemoryPolicy => "c3",
            Self::ToolGuidance => "c4",
            Self::Agents => "c5",
            Self::Skills => "c6",
            Self::Scripts => "c7",
            Self::Hooks => "c8",
        }
    }

    /// What the component is.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "system prompt overlay",
            Self::Guardrails => "guardrails",
            Self::MemoryPolicy => "memory policy",
            Self::ToolGuidance => "tool guidance",
            Self::Agents => "subagent definitions",
            Self::Skills => "skills",
            Self::Scripts => "scripts",
            Self::Hooks => "hooks",
        }
    }
}

const NAME: &str = "[a-z0-9][a-z0-9_-]{0,63}";
const FILE: &str = r"[a-z0-9_][a-z0-9_.-]{0,63}\.(?:py|sh)";

static AGENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^agents/{NAME}/AGENT\\.md$")).expect("agent path"));
static SKILL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^skills/{NAME}/(?:SKILL\\.md|scripts/{FILE})$")).expect("skill path")
});
static SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^scripts/{FILE}$")).expect("script path"));

/// The component a normalized path belongs to; `None` is off-surface.
#[must_use]
pub fn classify(path: &str) -> Option<Component> {
    match path {
        "SYSTEM.md" => Some(Component::System),
        "GUARDRAILS.md" => Some(Component::Guardrails),
        "MEMORY_POLICY.md" => Some(Component::MemoryPolicy),
        "TOOL_GUIDANCE.md" => Some(Component::ToolGuidance),
        "hooks.toml" => Some(Component::Hooks),
        _ if AGENT.is_match(path) => Some(Component::Agents),
        _ if SKILL.is_match(path) => Some(Component::Skills),
        _ if SCRIPT.is_match(path) => Some(Component::Scripts),
        _ => None,
    }
}

/// Whether `path` is an executable script (c6 skill script or c7).
#[must_use]
pub fn is_script(path: &str) -> bool {
    pmt_config::harness::is_script(path)
        && matches!(classify(path), Some(Component::Scripts | Component::Skills))
}

/// Most paths an off-surface error names before `(+N more)`.
pub const LISTED: usize = 6;

/// `a, b, c (+N more)`: at most [`LISTED`] items.
#[must_use]
pub fn listing(items: &[String]) -> String {
    let shown = items
        .iter()
        .take(LISTED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match items.len().saturating_sub(LISTED) {
        0 => shown,
        more => format!("{shown} (+{more} more)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_component_number_names_exactly_one_component() {
        let numbers: std::collections::BTreeSet<_> =
            Component::ALL.iter().map(|c| c.number()).collect();
        assert_eq!(numbers.len(), Component::ALL.len());
    }

    #[test]
    fn surface_paths_classify_and_others_do_not() {
        assert_eq!(classify("SYSTEM.md"), Some(Component::System));
        assert_eq!(
            classify("agents/reviewer/AGENT.md"),
            Some(Component::Agents)
        );
        assert_eq!(classify("skills/recon/SKILL.md"), Some(Component::Skills));
        assert_eq!(
            classify("skills/recon/scripts/sum.py"),
            Some(Component::Skills)
        );
        assert_eq!(classify("scripts/clean.sh"), Some(Component::Scripts));
        assert_eq!(classify("hooks.toml"), Some(Component::Hooks));
        for off in [
            "memories/MEMORY.md",
            "scripts/Clean.py",
            "config.toml",
            "secrets.toml",
            "scripts/run.rb",
            "skills/recon/notes.md",
            "agents/Reviewer/AGENT.md",
        ] {
            assert_eq!(classify(off), None, "{off}");
        }
    }

    #[test]
    fn listings_stop_at_six() {
        let items: Vec<String> = (1..=8).map(|i| format!("p{i}")).collect();
        assert_eq!(listing(&items), "p1, p2, p3, p4, p5, p6 (+2 more)");
        assert_eq!(listing(&items[..2]), "p1, p2");
    }
}
