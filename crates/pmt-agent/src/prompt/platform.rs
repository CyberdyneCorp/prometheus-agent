//! Platform hints (spec: agent-turn-loop, Platform hint overrides): the
//! built-in hint of the session's surface, then `prompt.platform_hints.
//! <platform>` (`replace` substitutes it, `append` follows it).

use std::collections::BTreeMap;

use pmt_config::schema::PlatformHint;

/// Built-in hints by platform.
pub const BUILTIN: &[(&str, &str)] = &[
    (
        "cli",
        "You are running in a terminal. Format answers as plain text or light Markdown; keep code in fenced blocks.",
    ),
    (
        "tui",
        "You are running in a terminal UI that renders Markdown; keep code in fenced blocks.",
    ),
    (
        "desktop",
        "You are running in the desktop app, which renders Markdown, code blocks and images.",
    ),
    (
        "acp",
        "You are running inside an editor through the Agent Client Protocol; keep answers focused on the open workspace.",
    ),
    (
        "api",
        "You are answering an API caller; reply with the requested content only.",
    ),
    (
        "cron",
        "You are running a scheduled job with nobody watching; finish the task without asking questions and report the outcome.",
    ),
    (
        "telegram",
        "You are chatting on Telegram. Keep replies short; Telegram renders basic Markdown.",
    ),
    (
        "discord",
        "You are chatting on Discord. Keep replies under 2000 characters; Discord renders Markdown.",
    ),
    (
        "slack",
        "You are chatting on Slack. Keep replies short; Slack uses its own mrkdwn formatting.",
    ),
    (
        "whatsapp",
        "You are chatting on WhatsApp. Keep replies short and use plain text.",
    ),
    (
        "signal",
        "You are chatting on Signal. Keep replies short and use plain text.",
    ),
    (
        "matrix",
        "You are chatting on Matrix, which renders Markdown.",
    ),
    (
        "email",
        "You are answering an email. Write a complete, polite reply in plain text.",
    ),
    (
        "sms",
        "You are answering by SMS. Keep replies very short and use plain text.",
    ),
];

/// The built-in hint of `platform`.
#[must_use]
pub fn builtin(platform: &str) -> Option<&'static str> {
    BUILTIN
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, hint)| *hint)
}

/// Every platform with a built-in hint.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTIN.iter().map(|(name, _)| *name)
}

/// The hint of `platform`: `registered` (an adapter's own hint) or the
/// built-in one, modified by the configured override.
#[must_use]
pub fn hint(
    platform: &str,
    registered: Option<&str>,
    overrides: &BTreeMap<String, PlatformHint>,
) -> Option<String> {
    let base = registered.or_else(|| builtin(platform));
    let Some(over) = overrides.get(platform) else {
        return base.map(str::to_owned);
    };
    let main = over.replace.as_deref().or(base);
    match (main, over.append.as_deref()) {
        (Some(main), Some(append)) => Some(format!("{main}\n{append}")),
        (Some(text), None) | (None, Some(text)) => Some(text.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_then_append() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "telegram".to_owned(),
            PlatformHint {
                replace: Some("Short replies.".into()),
                append: Some("Use Markdown.".into()),
            },
        );
        assert_eq!(
            hint("telegram", None, &overrides).as_deref(),
            Some("Short replies.\nUse Markdown.")
        );
        overrides.insert(
            "cli".to_owned(),
            PlatformHint {
                replace: None,
                append: Some("Be brief.".into()),
            },
        );
        let cli = hint("cli", None, &overrides).unwrap();
        assert!(cli.starts_with("You are running in a terminal.") && cli.ends_with("\nBe brief."));
        assert_eq!(hint("unknown", None, &BTreeMap::new()), None);
        assert_eq!(
            hint("matrix", Some("Adapter hint."), &BTreeMap::new()).as_deref(),
            Some("Adapter hint.")
        );
    }
}
