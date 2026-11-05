//! The plugin rights of the capability vocabulary: `plugin.<right>[:<target>]`
//! (spec: plugin-system, Capability declarations; security-boundaries,
//! Typed capability vocabulary).
//!
//! A plugin's manifest asks for its authority in `[capabilities]`; the keys
//! that have a general capability (`network`, `secrets`, `fs_*`, `llm`,
//! `tools_*`, `subprocess`, `ui`) map onto it, and the keys that only make
//! sense for plugins (hooks, slash commands, prompt sections, providers,
//! message injection...) map onto one of these rights, so every plugin
//! grant is one row of `grants.toml` like any other.

use std::fmt;

/// A right only plugins hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginRight {
    /// `plugin.hook:<hook name>`: subscribe to a hook.
    Hook,
    /// `plugin.command:<name>`: register a slash command.
    Command,
    /// `plugin.cli_command:<name>`: register `prometheus plugins run <id> <name>`.
    CliCommand,
    /// `plugin.prompt_section:<max chars>`: register system-prompt sections.
    PromptSection,
    /// `plugin.provider:<id>`: register a model or provider object.
    Provider,
    /// `plugin.provider_override:<id>`: replace a built-in provider.
    ProviderOverride,
    /// `plugin.approval_transport`: deliver approval prompts.
    ApprovalTransport,
    /// `plugin.inject_messages`: inject messages into sessions.
    InjectMessages,
    /// `plugin.platform_action:<verb>`: `add_reaction`, `set_thread_title`.
    PlatformAction,
    /// `plugin.llm_model:<model|*>`: name a model in `llm.complete`.
    LlmModel,
    /// `plugin.llm_provider_override`: name a provider in `llm.complete`.
    LlmProviderOverride,
}

/// Every right with its name and whether it takes a target.
const RIGHTS: &[(PluginRight, &str, bool)] = &[
    (PluginRight::Hook, "hook", true),
    (PluginRight::Command, "command", true),
    (PluginRight::CliCommand, "cli_command", true),
    (PluginRight::PromptSection, "prompt_section", true),
    (PluginRight::Provider, "provider", true),
    (PluginRight::ProviderOverride, "provider_override", true),
    (PluginRight::ApprovalTransport, "approval_transport", false),
    (PluginRight::InjectMessages, "inject_messages", false),
    (PluginRight::PlatformAction, "platform_action", true),
    (PluginRight::LlmModel, "llm_model", true),
    (
        PluginRight::LlmProviderOverride,
        "llm_provider_override",
        false,
    ),
];

/// The verbs `plugin.platform_action` accepts.
pub const PLATFORM_ACTIONS: &[&str] = &["add_reaction", "set_thread_title"];

/// The largest system-prompt section a plugin may register.
pub const MAX_PROMPT_SECTION_CHARS: u64 = 4000;

impl PluginRight {
    /// The right's name after `plugin.`.
    #[must_use]
    pub fn name(self) -> &'static str {
        RIGHTS
            .iter()
            .find(|(right, _, _)| *right == self)
            .map_or("", |(_, name, _)| name)
    }

    fn takes_target(self) -> bool {
        RIGHTS
            .iter()
            .any(|(right, _, target)| *right == self && *target)
    }

    /// Whether `target` is well formed for this right.
    fn accepts(self, target: &str) -> bool {
        match self {
            Self::PromptSection => target
                .parse::<u64>()
                .is_ok_and(|n| (1..=MAX_PROMPT_SECTION_CHARS).contains(&n)),
            Self::PlatformAction => PLATFORM_ACTIONS.contains(&target),
            Self::LlmModel => target == "*" || is_target_word(target),
            _ => is_target_word(target),
        }
    }
}

impl fmt::Display for PluginRight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Parses the part after `plugin.`: `<right>` or `<right>:<target>`.
pub(super) fn parse(rest: &str) -> Option<(PluginRight, Option<String>)> {
    let (name, target) = match rest.split_once(':') {
        Some((name, target)) => (name, Some(target)),
        None => (rest, None),
    };
    let (right, _, takes) = RIGHTS.iter().find(|(_, n, _)| *n == name)?;
    match (takes, target) {
        (true, Some(target)) if right.accepts(target) => Some((*right, Some(target.to_owned()))),
        (false, None) => Some((*right, None)),
        _ => None,
    }
}

/// Whether holding `(right, a)` grants at least `(right, b)`.
pub(super) fn covers(right: PluginRight, a: Option<&str>, b: Option<&str>) -> bool {
    match (right, a, b) {
        (PluginRight::PromptSection, Some(a), Some(b)) => {
            matches!((a.parse::<u64>(), b.parse::<u64>()), (Ok(a), Ok(b)) if a >= b)
        }
        (PluginRight::LlmModel, Some("*"), Some(_)) => true,
        _ => a == b && (a.is_some() == right.takes_target()),
    }
}

/// Hook, command, provider and model names: letters, digits, `_`, `-`,
/// `.` and `/` (`openrouter/gpt-5`).
fn is_target_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
}
