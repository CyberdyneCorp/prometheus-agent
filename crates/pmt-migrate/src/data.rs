//! Facts about Hermes that the migrator needs and Hermes keeps in code:
//! its config defaults (a value equal to one is `default_omitted`), its
//! credential and setting environment variable names, and its built-in
//! personalities. They are generated from a Hermes checkout by
//! `data/generate.py` (the source revision is recorded in each file) and
//! embedded, so the migrator never runs Hermes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct DefaultsFile {
    config_version: u32,
    defaults: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct EnvFile {
    secrets: BTreeSet<String>,
    settings: BTreeSet<String>,
}

#[derive(Deserialize)]
struct PersonalitiesFile {
    personalities: BTreeMap<String, String>,
}

fn load<T: for<'de> Deserialize<'de>>(name: &str, text: &str) -> T {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("embedded {name}: {e}"))
}

static DEFAULTS: LazyLock<DefaultsFile> = LazyLock::new(|| {
    load(
        "hermes-defaults.json",
        include_str!("../data/hermes-defaults.json"),
    )
});
static ENV: LazyLock<EnvFile> =
    LazyLock::new(|| load("hermes-env.json", include_str!("../data/hermes-env.json")));
static PERSONALITIES: LazyLock<PersonalitiesFile> = LazyLock::new(|| {
    load(
        "hermes-personalities.json",
        include_str!("../data/hermes-personalities.json"),
    )
});

/// The newest Hermes `_config_version` this migrator knows.
#[must_use]
pub(crate) fn known_config_version() -> u32 {
    DEFAULTS.config_version
}

/// The Hermes default of a dotted key (leaves only; an empty mapping is
/// a leaf).
#[must_use]
pub(crate) fn default_of(key: &str) -> Option<&'static Value> {
    DEFAULTS.defaults.get(key)
}

/// Whether Hermes registers `name` as a credential (a password field).
#[must_use]
pub(crate) fn is_hermes_secret(name: &str) -> bool {
    ENV.secrets.contains(name)
}

/// Whether Hermes registers `name` as a non-secret setting.
#[must_use]
pub(crate) fn is_hermes_setting(name: &str) -> bool {
    ENV.settings.contains(name)
}

/// A Hermes built-in personality's prompt text.
#[must_use]
pub(crate) fn builtin_personality(name: &str) -> Option<&'static str> {
    PERSONALITIES.personalities.get(name).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_facts_load() {
        assert_eq!(known_config_version(), 52);
        assert_eq!(
            default_of("tool_output.max_bytes"),
            Some(&Value::from(50_000))
        );
        assert!(is_hermes_secret("ANTHROPIC_API_KEY"));
        assert!(is_hermes_setting("TELEGRAM_ALLOWED_USERS"));
        assert!(builtin_personality("pirate").is_some());
    }
}
