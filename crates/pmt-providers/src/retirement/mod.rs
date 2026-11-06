//! The model retirement table (spec: model-providers, Model retirement
//! table; configuration-and-profiles, Retired model rewrite).
//!
//! The built-in table maps each deprecated or retired `<provider>/<model>`
//! id to its status, retirement date and replacement; it is updated with
//! each release. `[models."<provider>/<model>"] replacement` replaces an
//! entry's replacement. A `retired` id refuses a new session (exit 78
//! naming the replacement and `prometheus config fix-models`); a
//! `deprecated` one is still used, with one WARN per process.

mod scan;

use std::collections::BTreeSet;
use std::sync::Mutex;

use pmt_config::Config;
use pmt_config::schema::ModelEntry;
use pmt_core::error::{Error, codes};
use serde::Serialize;

pub use scan::{Finding, Rewrite, scan};

/// Whether a model still answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Still served, scheduled for retirement.
    Deprecated,
    /// No longer served.
    Retired,
}

impl Status {
    /// The table spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deprecated => "deprecated",
            Self::Retired => "retired",
        }
    }
}

/// One table entry, with the profile's replacement override applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Retirement {
    /// `<provider>/<model>`.
    pub id: String,
    /// Deprecated or retired.
    pub status: Status,
    /// Retirement date (`YYYY-MM-DD`), when announced.
    pub date: Option<String>,
    /// `<provider>/<model>` to use instead.
    pub replacement: Option<String>,
}

use Status::{Deprecated, Retired};

/// `(id, status, retirement date, replacement)`.
const TABLE: &[(&str, Status, Option<&str>, Option<&str>)] = &[
    (
        "anthropic/claude-2.0",
        Retired,
        Some("2025-07-21"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-2.1",
        Retired,
        Some("2025-07-21"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-3-sonnet-20240229",
        Retired,
        Some("2025-07-21"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-3-5-sonnet-20240620",
        Retired,
        Some("2025-10-22"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-3-5-sonnet-20241022",
        Retired,
        Some("2025-10-22"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-3-opus-20240229",
        Retired,
        Some("2026-01-05"),
        Some("anthropic/claude-opus-4-1"),
    ),
    (
        "anthropic/claude-3-7-sonnet-20250219",
        Deprecated,
        Some("2026-02-19"),
        Some("anthropic/claude-sonnet-4-5"),
    ),
    (
        "anthropic/claude-3-5-haiku-20241022",
        Deprecated,
        Some("2026-02-19"),
        Some("anthropic/claude-haiku-4-5"),
    ),
    (
        "openai-api/gpt-4.5-preview",
        Retired,
        Some("2025-07-14"),
        Some("openai-api/gpt-5"),
    ),
    (
        "openai-api/gpt-4-32k",
        Retired,
        Some("2025-06-06"),
        Some("openai-api/gpt-5"),
    ),
    (
        "openai-api/o1-preview",
        Retired,
        Some("2025-07-28"),
        Some("openai-api/gpt-5"),
    ),
    (
        "openai-api/o1-mini",
        Retired,
        Some("2025-10-27"),
        Some("openai-api/gpt-5-mini"),
    ),
    (
        "xai/grok-beta",
        Retired,
        Some("2025-09-15"),
        Some("xai/grok-4"),
    ),
    (
        "xai/grok-vision-beta",
        Retired,
        Some("2025-09-15"),
        Some("xai/grok-4"),
    ),
    (
        "xai/grok-2",
        Retired,
        Some("2025-09-15"),
        Some("xai/grok-4"),
    ),
    (
        "xai/grok-2-1212",
        Retired,
        Some("2025-09-15"),
        Some("xai/grok-4"),
    ),
    ("xai/grok-3-mini", Deprecated, None, Some("xai/grok-4-fast")),
];

/// The entry of `provider`/`model`, with the profile's replacement.
#[must_use]
pub fn lookup(config: &Config, provider: &str, model: &str) -> Option<Retirement> {
    let id = format!("{provider}/{model}");
    let (_, status, date, replacement) = TABLE
        .iter()
        .find(|(entry, ..)| entry.eq_ignore_ascii_case(&id))?;
    let replacement = override_of(config, &id).or_else(|| replacement.map(str::to_owned));
    Some(Retirement {
        id,
        status: *status,
        date: date.map(str::to_owned),
        replacement,
    })
}

fn override_of(config: &Config, id: &str) -> Option<String> {
    match config.models.get(id)? {
        ModelEntry::Route(route) => route.replacement.clone(),
        ModelEntry::Alias(_) => None,
    }
}

/// Checks the model a new session or route resolves to: a retired model
/// is refused; a deprecated one is used with one WARN per process.
///
/// # Errors
///
/// `model.retired` (exit 78) naming the replacement and the rewrite
/// command.
pub fn check(config: &Config, provider: &str, model: &str) -> Result<(), Error> {
    let Some(entry) = lookup(config, provider, model) else {
        return Ok(());
    };
    let instead = entry
        .replacement
        .as_deref()
        .map_or_else(String::new, |r| format!("; use {r} instead"));
    match entry.status {
        Retired => Err(Error::new(
            &codes::MODEL_RETIRED,
            format!(
                "model {} is retired{}{instead}",
                entry.id,
                entry
                    .date
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" since {d}"))
            ),
        )
        .with_hint("rewrite the config with: prometheus config fix-models --apply")),
        Deprecated => {
            warn_once(&entry, &instead);
            Ok(())
        }
    }
}

/// [`check`] for a model reference as config holds it: `<provider>/<model>`
/// with a registered provider name, else a model of `provider`. Lets a
/// session refuse a retired `model.default` before automatic provider
/// detection runs.
///
/// # Errors
///
/// As [`check`].
pub fn check_reference(
    config: &Config,
    registry: &crate::registry::ProviderRegistry,
    reference: &str,
    provider: Option<&str>,
) -> Result<(), Error> {
    match scan::interpret(registry, reference, provider) {
        Some((provider, model, _)) => check(config, &provider, &model),
        None => Ok(()),
    }
}

fn warn_once(entry: &Retirement, instead: &str) {
    static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(entry.id.clone());
    if first {
        tracing::warn!(
            model = %entry.id,
            date = entry.date.as_deref().unwrap_or("unannounced"),
            "model {} is deprecated{instead}; run prometheus config fix-models",
            entry.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_ids_are_refused_naming_the_replacement_and_the_rewrite() {
        let err = check(&Config::default(), "xai", "grok-2").unwrap_err();
        assert_eq!(err.code().code, "model.retired");
        assert_eq!(err.exit_code(), 78);
        assert!(err.message().contains("xai/grok-4"), "{}", err.message());
        assert!(err.hint().unwrap().contains("prometheus config fix-models"));
        check(&Config::default(), "xai", "grok-3-mini").unwrap();
        check(&Config::default(), "xai", "grok-4").unwrap();
    }

    #[test]
    fn the_profile_overrides_a_replacement() {
        let config: Config =
            toml::from_str("[models.\"xai/grok-2\"]\nreplacement = \"xai/grok-4-fast\"\n").unwrap();
        let entry = lookup(&config, "xai", "grok-2").unwrap();
        assert_eq!(entry.replacement.as_deref(), Some("xai/grok-4-fast"));
        assert_eq!(entry.status, Status::Retired);
    }
}
