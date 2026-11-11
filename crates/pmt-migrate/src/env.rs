//! A Hermes profile's `.env`, re-sorted (spec: hermes-migration, `.env`
//! re-sorted into secrets and config). Only the file is read, never the
//! shell environment. Each entry is a secret, a setting with a config key,
//! a setting with none, a process-control variable, or (unrecognised) a
//! secret with reason `unclassified`.

use std::collections::BTreeMap;

use crate::data;

/// One `NAME=value` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    /// The name.
    pub name: String,
    /// The value (quotes removed).
    pub value: String,
}

/// Parses dotenv text: `NAME=value`, optional `export `, `#` comments,
/// single or double quotes (double quotes honour `\n`, `\"`, `\\`); the
/// last assignment of a name wins.
#[must_use]
pub fn parse(text: &str) -> Vec<EnvEntry> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, raw)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if !entries.contains_key(name) {
            order.push(name.to_owned());
        }
        entries.insert(name.to_owned(), unquote(raw.trim()));
    }
    order
        .into_iter()
        .map(|name| {
            let value = entries.remove(&name).unwrap_or_default();
            EnvEntry { name, value }
        })
        .collect()
}

fn unquote(raw: &str) -> String {
    if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return inner.to_owned();
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    // An unquoted value ends at an inline comment.
    raw.split_once(" #")
        .map_or(raw, |(v, _)| v)
        .trim()
        .to_owned()
}

/// What an entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// A credential: stored under the same name.
    Secret,
    /// Unrecognised: stored as a secret, reason `unclassified`.
    Unclassified,
    /// A behaviour setting with this config key and value transform.
    Setting(&'static str, Transform),
    /// A behaviour setting without a Prometheus key.
    Unsupported,
    /// A messaging-platform setting (the gateway component owns it).
    Platform,
    /// A process-control variable, dropped.
    ProcessControl,
    /// `HERMES_LANGUAGE`: the identity component maps it to
    /// `display.language` with the profile's overlays.
    Language,
}

/// How a setting's text becomes a config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// A TOML string.
    Text,
    /// A TOML integer.
    Integer,
    /// One of `queue`, `steer`, `interrupt`.
    BusyMode,
}

/// Known non-secret settings with a Prometheus key.
const SETTINGS: &[(&str, &str, Transform)] = &[
    ("MESSAGING_CWD", "terminal.cwd", Transform::Text),
    ("TERMINAL_CWD", "terminal.cwd", Transform::Text),
    ("HERMES_TIMEZONE", "cron.timezone", Transform::Text),
    (
        "HERMES_CRON_MAX_PARALLEL",
        "cron.max_parallel_jobs",
        Transform::Integer,
    ),
    (
        "HERMES_GATEWAY_BUSY_INPUT_MODE",
        "busy.input_mode",
        Transform::BusyMode,
    ),
];

/// Known Hermes behaviour variables Prometheus has no key for.
const UNSUPPORTED: &[&str] = &[
    "HERMES_SEARCH_SLOW_MS",
    "HERMES_EPHEMERAL_SYSTEM_PROMPT",
    "HERMES_PREFILL_MESSAGES_FILE",
    "HERMES_MAX_ITERATIONS",
    "HERMES_YOLO_MODE",
    "HERMES_DEBUG",
    "HERMES_QUIET",
];

/// Variables that steer the Hermes process itself.
const PROCESS_CONTROL: &[&str] = &[
    "HERMES_HOME",
    "HERMES_PROFILE",
    "HERMES_CONFIG_BACKEND",
    "HERMES_RUNTIME_DIR",
    "HERMES_LOG_LEVEL",
    "PATH",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "HOME",
    "USER",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TZ",
];

/// Platform prefixes whose settings belong to the gateway component.
pub const PLATFORM_PREFIXES: &[&str] = &[
    "TELEGRAM_",
    "DISCORD_",
    "SLACK_",
    "SIGNAL_",
    "MATRIX_",
    "EMAIL_",
    "WHATSAPP_",
    "WEBHOOK_",
    "MATTERMOST_",
    "DINGTALK_",
    "FEISHU_",
    "WECOM_",
    "WEIXIN_",
    "BLUEBUBBLES_",
    "QQ_",
    "QQBOT_",
    "YUANBAO_",
    "LINE_",
    "SMS_",
    "NTFY_",
    "IRC_",
    "BUZZ_",
    "SIMPLEX_",
    "PHOTON_",
    "TEAMS_",
    "GOOGLE_CHAT_",
    "GATEWAY_",
    "A2A_",
    "API_SERVER_",
];

/// Whether `name` looks like a credential.
#[must_use]
pub fn secret_shaped(name: &str) -> bool {
    ["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_KEY"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Classifies `name`; `known_secret` answers for names Prometheus
/// providers, adapters and tools register as credentials.
#[must_use]
pub fn classify(name: &str, known_secret: &dyn Fn(&str) -> bool) -> Class {
    if PROCESS_CONTROL.contains(&name) {
        return Class::ProcessControl;
    }
    if name == crate::locale::ENV_NAME {
        return Class::Language;
    }
    if let Some((_, key, transform)) = SETTINGS.iter().find(|(n, _, _)| *n == name) {
        return Class::Setting(key, *transform);
    }
    if data::is_hermes_secret(name) || known_secret(name) || secret_shaped(name) {
        return Class::Secret;
    }
    if PLATFORM_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Class::Platform;
    }
    if UNSUPPORTED.contains(&name) || data::is_hermes_setting(name) || name.starts_with("HERMES_") {
        return Class::Unsupported;
    }
    Class::Unclassified
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_syntax_parses() {
        let entries =
            parse("# c\nexport A=1\nB='two words'\nC=\"x\\ny\"\nD=plain # note\nA=3\nbad line\n");
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("A", "3"),
                ("B", "two words"),
                ("C", "x\ny"),
                ("D", "plain")
            ]
        );
    }

    #[test]
    fn names_sort_into_classes() {
        let none = |_: &str| false;
        assert_eq!(classify("ANTHROPIC_API_KEY", &none), Class::Secret);
        assert_eq!(classify("ACME_WIDGET", &none), Class::Unclassified);
        assert_eq!(
            classify("HERMES_CRON_MAX_PARALLEL", &none),
            Class::Setting("cron.max_parallel_jobs", Transform::Integer)
        );
        assert_eq!(classify("HERMES_SEARCH_SLOW_MS", &none), Class::Unsupported);
        assert_eq!(classify("PYTHONPATH", &none), Class::ProcessControl);
        assert_eq!(classify("TELEGRAM_ALLOWED_USERS", &none), Class::Platform);
        assert_eq!(classify("TELEGRAM_BOT_TOKEN", &none), Class::Secret);
    }
}
