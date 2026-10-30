//! The context length detection chain (spec: model-providers, Context
//! length detection chain): the `[models."<provider>/<model>"]
//! context_window` pin, the provider's listing, a local server probe, the
//! discovered-value cache in `state.db`, the built-in catalog, the
//! models.dev snapshot, family defaults by model-name prefix, and finally
//! 128000 tokens. The winner is recorded as the window's source (shown by
//! `prometheus model explain`; a pin is labelled `(pinned)`).

use serde::Serialize;

use super::DEFAULT_CONTEXT_WINDOW;

/// Where a model's context window came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// `[models."<provider>/<model>"] context_window`.
    Pinned,
    /// The provider's model listing.
    Listing,
    /// A local server probe this session.
    Probe,
    /// The discovered-value cache (an earlier listing or probe).
    Discovered,
    /// The built-in catalog.
    Builtin,
    /// The models.dev snapshot.
    ModelsDev,
    /// A family default by model-name prefix.
    Family,
    /// Nothing: 128000 tokens.
    Default,
}

impl ContextSource {
    /// The snake-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Listing => "listing",
            Self::Probe => "probe",
            Self::Discovered => "discovered",
            Self::Builtin => "builtin",
            Self::ModelsDev => "models_dev",
            Self::Family => "family",
            Self::Default => "default",
        }
    }

    /// Whether the window is a guess (family default or the fallback).
    #[must_use]
    pub const fn is_guess(self) -> bool {
        matches!(self, Self::Family | Self::Default)
    }
}

/// `window` as the status line and pickers show it: `200000 (pinned)`.
#[must_use]
pub fn label(window: u64, source: ContextSource) -> String {
    if source == ContextSource::Pinned {
        format!("{window} (pinned)")
    } else {
        window.to_string()
    }
}

/// The values the sources before models.dev reported.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Found {
    pub pinned: Option<u64>,
    pub listed: Option<u64>,
    pub probed: Option<u64>,
    pub discovered: Option<u64>,
    pub builtin: Option<u64>,
}

/// The first source with a value; `models_dev` is consulted only when
/// everything before it is silent.
pub(super) fn resolve(
    found: &Found,
    models_dev: impl FnOnce() -> Option<u64>,
    model: &str,
) -> (u64, ContextSource) {
    let ordered = [
        (found.pinned, ContextSource::Pinned),
        (found.listed, ContextSource::Listing),
        (found.probed, ContextSource::Probe),
        (found.discovered, ContextSource::Discovered),
        (found.builtin, ContextSource::Builtin),
    ];
    if let Some((window, source)) = ordered
        .into_iter()
        .find_map(|(window, source)| window.map(|w| (w, source)))
    {
        return (window, source);
    }
    if let Some(window) = models_dev() {
        return (window, ContextSource::ModelsDev);
    }
    family(model).map_or((DEFAULT_CONTEXT_WINDOW, ContextSource::Default), |w| {
        (w, ContextSource::Family)
    })
}

/// Family defaults: the usual window of a model line, by the prefix of the
/// model name (after any `vendor/` prefix), longest prefix first.
const FAMILIES: &[(&str, u64)] = &[
    ("claude-", 200_000),
    ("gpt-4.1", 1_047_576),
    ("gpt-4o", 128_000),
    ("gpt-5", 400_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("gemini-", 1_048_576),
    ("grok-4", 256_000),
    ("grok-3", 131_072),
    ("deepseek", 128_000),
    ("kimi-k2", 262_144),
    ("glm-4", 131_072),
    ("minimax-m", 204_800),
    ("qwen3", 32_768),
    ("llama-3", 131_072),
    ("llama3", 131_072),
    ("mistral", 32_768),
    ("hermes-4", 131_072),
];

fn family(model: &str) -> Option<u64> {
    let name = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    FAMILIES
        .iter()
        .filter(|(prefix, _)| name.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, window)| *window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_source_wins_over_every_later_one() {
        let all = Found {
            pinned: Some(1),
            listed: Some(2),
            probed: Some(3),
            discovered: Some(4),
            builtin: Some(5),
        };
        let mut found = all;
        let order = [
            ContextSource::Pinned,
            ContextSource::Listing,
            ContextSource::Probe,
            ContextSource::Discovered,
            ContextSource::Builtin,
        ];
        for (expected, source) in (1..).zip(order) {
            assert_eq!(resolve(&found, || Some(6), "claude-x"), (expected, source));
            match source {
                ContextSource::Pinned => found.pinned = None,
                ContextSource::Listing => found.listed = None,
                ContextSource::Probe => found.probed = None,
                ContextSource::Discovered => found.discovered = None,
                _ => found.builtin = None,
            }
        }
        assert_eq!(
            resolve(&found, || Some(6), "claude-x"),
            (6, ContextSource::ModelsDev)
        );
        assert_eq!(
            resolve(&found, || None, "anthropic/claude-x"),
            (200_000, ContextSource::Family)
        );
        assert_eq!(
            resolve(&found, || None, "mystery"),
            (128_000, ContextSource::Default)
        );
    }

    #[test]
    fn pinned_windows_are_labelled() {
        assert_eq!(label(200_000, ContextSource::Pinned), "200000 (pinned)");
        assert_eq!(label(200_000, ContextSource::Listing), "200000");
    }
}
