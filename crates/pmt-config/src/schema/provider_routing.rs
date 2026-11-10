//! OpenRouter routing preferences and the fast mode service tier (spec:
//! model-providers, OpenRouter provider routing preferences; Fast mode
//! service tier).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How OpenRouter orders the upstream providers of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoutingSort {
    /// Cheapest first.
    Price,
    /// Highest tokens per second first.
    Throughput,
    /// Lowest time to first token first.
    Latency,
}

impl RoutingSort {
    /// The request spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Price => "price",
            Self::Throughput => "throughput",
            Self::Latency => "latency",
        }
    }
}

/// Whether upstream providers that store or train on prompts may serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataCollection {
    /// Any provider.
    Allow,
    /// Only providers that neither store nor train on prompts.
    Deny,
}

impl DataCollection {
    /// The request spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// OpenRouter's `provider` request object, as preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingPrefs {
    /// Order upstream providers by price, throughput or latency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<RoutingSort>,
    /// Only these upstream providers may serve.
    pub only: Vec<String>,
    /// These upstream providers never serve.
    pub ignore: Vec<String>,
    /// Upstream providers tried first, in this order.
    pub order: Vec<String>,
    /// Only upstream providers that support every request parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    /// Whether upstream providers that store or train on prompts may serve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
}

impl RoutingPrefs {
    /// `self` with every field `over` sets replaced by it.
    #[must_use]
    pub fn overlaid(&self, over: &Self) -> Self {
        let list = |mine: &Vec<String>, theirs: &Vec<String>| {
            if theirs.is_empty() {
                mine.clone()
            } else {
                theirs.clone()
            }
        };
        Self {
            sort: over.sort.or(self.sort),
            only: list(&self.only, &over.only),
            ignore: list(&self.ignore, &over.ignore),
            order: list(&self.order, &over.order),
            require_parameters: over.require_parameters.or(self.require_parameters),
            data_collection: over.data_collection.or(self.data_collection),
        }
    }

    /// Whether no preference is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The `[provider_routing]` section: OpenRouter routing preferences sent
/// only on `openrouter` routes, with per-model overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ProviderRoutingConfig {
    /// Order upstream providers by price, throughput or latency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<RoutingSort>,
    /// Only these upstream providers may serve.
    pub only: Vec<String>,
    /// These upstream providers never serve.
    pub ignore: Vec<String>,
    /// Upstream providers tried first, in this order.
    pub order: Vec<String>,
    /// Only upstream providers that support every request parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    /// Whether upstream providers that store or train on prompts may serve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    /// Per-model overrides (`[provider_routing.models."<model>"]`), matched
    /// case-insensitively ignoring separators; each set field replaces the
    /// section's.
    pub models: BTreeMap<String, RoutingPrefs>,
}

impl ProviderRoutingConfig {
    /// The section's own preferences (without per-model overrides).
    #[must_use]
    pub fn prefs(&self) -> RoutingPrefs {
        RoutingPrefs {
            sort: self.sort,
            only: self.only.clone(),
            ignore: self.ignore.clone(),
            order: self.order.clone(),
            require_parameters: self.require_parameters,
            data_collection: self.data_collection,
        }
    }
}

/// The default `openrouter.min_coding_score`.
pub const DEFAULT_MIN_CODING_SCORE: f64 = 0.65;

/// The `[openrouter]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct OpenRouterConfig {
    /// Lowest coding score the `openrouter/pareto-code` router may pick a
    /// model below (0.0-1.0); an empty string sends no `pareto-router`
    /// plugin block.
    #[serde(
        serialize_with = "score_out",
        deserialize_with = "score_in",
        default = "default_score"
    )]
    #[schemars(with = "ScoreSchema")]
    pub min_coding_score: Option<f64>,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            min_coding_score: default_score(),
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // serde `default` needs the field type.
const fn default_score() -> Option<f64> {
    Some(DEFAULT_MIN_CODING_SCORE)
}

/// A score or the empty string (no score).
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)] // Only its schema is used.
enum ScoreSchema {
    /// A score.
    Score(#[schemars(range(min = 0.0, max = 1.0))] f64),
    /// No score: the empty string.
    #[schemars(extend("maxLength" = 0))]
    Empty(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScoreValue {
    Number(f64),
    Text(String),
}

fn score_in<'de, D: Deserializer<'de>>(de: D) -> Result<Option<f64>, D::Error> {
    match ScoreValue::deserialize(de)? {
        ScoreValue::Number(n) if (0.0..=1.0).contains(&n) => Ok(Some(n)),
        ScoreValue::Number(n) => Err(serde::de::Error::custom(format!(
            "openrouter.min_coding_score {n} is outside 0.0-1.0"
        ))),
        ScoreValue::Text(t) if t.trim().is_empty() => Ok(None),
        ScoreValue::Text(t) => t.trim().parse::<f64>().map_or_else(
            |_| {
                Err(serde::de::Error::custom(format!(
                    "openrouter.min_coding_score '{t}' is not a number"
                )))
            },
            |n| {
                if (0.0..=1.0).contains(&n) {
                    Ok(Some(n))
                } else {
                    Err(serde::de::Error::custom(format!(
                        "openrouter.min_coding_score {n} is outside 0.0-1.0"
                    )))
                }
            },
        ),
    }
}

#[allow(clippy::ref_option)] // serde `serialize_with` passes `&Option`.
fn score_out<S: Serializer>(score: &Option<f64>, ser: S) -> Result<S::Ok, S::Error> {
    match score {
        Some(n) => ser.serialize_f64(*n),
        None => ser.serialize_str(""),
    }
}

/// `agent.fast_mode`: when requests ask the provider's priority tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FastMode {
    /// Never.
    #[default]
    Normal,
    /// Every request.
    Fast,
    /// Requests within the first `agent.fast_auto_secs` of each turn.
    Auto,
    /// That window on the first turn of a session only.
    Cold,
    /// Every request, on the provider's fastest tier where one exists
    /// (otherwise as `fast`).
    Ultrafast,
}

impl FastMode {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Fast => "fast",
            Self::Auto => "auto",
            Self::Cold => "cold",
            Self::Ultrafast => "ultrafast",
        }
    }

    /// Parses the config spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Normal,
            Self::Fast,
            Self::Auto,
            Self::Cold,
            Self::Ultrafast,
        ]
        .into_iter()
        .find(|m| m.as_str().eq_ignore_ascii_case(text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, Serialize)]
    struct Holder {
        #[serde(default)]
        openrouter: OpenRouterConfig,
    }

    #[test]
    fn coding_score_defaults_accepts_empty_and_refuses_out_of_range() {
        let none: Holder = toml::from_str("").unwrap();
        assert_eq!(none.openrouter.min_coding_score, Some(0.65));
        let set: Holder = toml::from_str("[openrouter]\nmin_coding_score = 0.8\n").unwrap();
        assert_eq!(set.openrouter.min_coding_score, Some(0.8));
        let empty: Holder = toml::from_str("[openrouter]\nmin_coding_score = \"\"\n").unwrap();
        assert_eq!(empty.openrouter.min_coding_score, None);
        let back: Holder = toml::from_str(&toml::to_string(&empty).unwrap()).unwrap();
        assert_eq!(back.openrouter.min_coding_score, None);
        assert!(toml::from_str::<Holder>("[openrouter]\nmin_coding_score = 1.5\n").is_err());
    }

    #[test]
    fn per_model_preferences_replace_only_the_fields_they_set() {
        let base = RoutingPrefs {
            sort: Some(RoutingSort::Price),
            only: vec!["anthropic".into()],
            ..RoutingPrefs::default()
        };
        let over = RoutingPrefs {
            sort: Some(RoutingSort::Latency),
            ..RoutingPrefs::default()
        };
        let merged = base.overlaid(&over);
        assert_eq!(merged.sort, Some(RoutingSort::Latency));
        assert_eq!(merged.only, vec!["anthropic".to_owned()]);
    }
}
