//! The `[moa]` section (spec: mixture-of-agents): named Mixture of Agents
//! presets, selected as models of the virtual provider `moa`, the
//! advisor privacy filter and the trace switch.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ReasoningLevel;

/// The traces directory under a profile.
pub const TRACE_DIR: &str = "moa-traces";

/// The preset every profile has unless it defines its own `default`.
pub const DEFAULT_PRESET: &str = "default";

/// How advisor text is redacted (`moa.privacy_filter`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum MoaPrivacyFilter {
    /// No redaction.
    #[default]
    #[serde(rename = "")]
    Off,
    /// Redact the advisor blocks shown to users and saved in traces; the
    /// aggregator still reads the raw text.
    #[serde(rename = "display")]
    Display,
    /// Also redact the guidance the aggregator reads.
    #[serde(rename = "full")]
    Full,
}

/// One model slot of a preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaSlot {
    /// Provider id or alias (never `moa`: presets do not nest).
    pub provider: String,
    /// Model id at that provider.
    pub model: String,
    /// Reasoning effort sent to this slot; unset uses the model's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningLevel>,
}

impl MoaSlot {
    /// `provider/model`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// When the references run (`fanout`): `user_turn`, `per_iteration`,
/// `every_n:N`, or `{ mode = "every_n", n = N }`. Any other value warns
/// once and behaves as `user_turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MoaFanout {
    /// `user_turn`, `per_iteration` or `every_n:N`.
    Text(String),
    /// `{ mode = "every_n", n = N }`.
    Table {
        /// The cadence mode.
        mode: String,
        /// Every how many iterations (`every_n`, N >= 2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<i64>,
    },
}

impl Default for MoaFanout {
    fn default() -> Self {
        Self::Text("user_turn".to_owned())
    }
}

/// One `[moa.presets.<name>]` preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPreset {
    /// The advisors, run concurrently without tools (at least one).
    pub reference_models: Vec<MoaSlot>,
    /// The model that reads the advisors' outputs and acts with the
    /// session's tools; the whole tool loop is billed to it.
    pub aggregator: MoaSlot,
    /// Temperature sent to the references (0.0-2.0); unset sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 2.0))]
    pub reference_temperature: Option<f64>,
    /// Temperature sent to the aggregator (0.0-2.0); unset sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 2.0))]
    pub aggregator_temperature: Option<f64>,
    /// When the references run within a turn.
    #[serde(default)]
    pub fanout: MoaFanout,
    /// `false` calls the aggregator alone, as a plain model.
    #[serde(default = "enabled")]
    pub enabled: bool,
}

const fn enabled() -> bool {
    true
}

impl MoaPreset {
    /// The built-in `default` preset.
    #[must_use]
    pub fn builtin() -> Self {
        let slot = |provider: &str, model: &str| MoaSlot {
            provider: provider.to_owned(),
            model: model.to_owned(),
            reasoning_effort: None,
        };
        Self {
            reference_models: vec![
                slot("openai-codex", "gpt-5.5"),
                slot("openrouter", "deepseek/deepseek-v4-pro"),
            ],
            aggregator: slot("openrouter", "anthropic/claude-opus-4.8"),
            reference_temperature: None,
            aggregator_temperature: None,
            fanout: MoaFanout::default(),
            enabled: true,
        }
    }
}

/// The `[moa]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct MoaConfig {
    /// The preset `/moa` runs and `moa configure` edits by default.
    pub default_preset: String,
    /// Redact advisor text: `""` (off), `display` (shown blocks and
    /// traces) or `full` (also the aggregator's guidance).
    pub privacy_filter: MoaPrivacyFilter,
    /// Append one JSONL record per MoA call to `moa-traces/<session>.jsonl`.
    pub save_traces: bool,
    /// Where traces go instead of `<profile>/moa-traces`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_dir: Option<String>,
    /// Named presets; the built-in `default` exists unless redefined here.
    pub presets: BTreeMap<String, MoaPreset>,
}

impl Default for MoaConfig {
    fn default() -> Self {
        Self {
            default_preset: DEFAULT_PRESET.to_owned(),
            privacy_filter: MoaPrivacyFilter::Off,
            save_traces: false,
            trace_dir: None,
            presets: BTreeMap::new(),
        }
    }
}

impl MoaConfig {
    /// Preset `name`: a configured one, else the built-in `default`.
    #[must_use]
    pub fn preset(&self, name: &str) -> Option<MoaPreset> {
        match self.presets.get(name) {
            Some(preset) => Some(preset.clone()),
            None if name == DEFAULT_PRESET => Some(MoaPreset::builtin()),
            None => None,
        }
    }

    /// Every preset name, sorted (the built-in `default` included).
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.presets.keys().cloned().collect();
        if !self.presets.contains_key(DEFAULT_PRESET) {
            names.push(DEFAULT_PRESET.to_owned());
            names.sort();
        }
        names
    }

    /// Where traces go: `moa.trace_dir` (relative to `profile_dir`), else
    /// `<profile_dir>/moa-traces`.
    #[must_use]
    pub fn trace_dir(&self, profile_dir: &std::path::Path) -> std::path::PathBuf {
        match &self.trace_dir {
            Some(dir) => profile_dir.join(dir),
            None => profile_dir.join(TRACE_DIR),
        }
    }

    /// Every preset with its name, sorted by name.
    #[must_use]
    pub fn all(&self) -> Vec<(String, MoaPreset)> {
        self.names()
            .into_iter()
            .filter_map(|name| self.preset(&name).map(|p| (name, p)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_default_exists_unless_redefined() {
        let config = MoaConfig::default();
        assert_eq!(config.names(), ["default"]);
        let default = config.preset("default").unwrap();
        assert_eq!(
            default.aggregator.label(),
            "openrouter/anthropic/claude-opus-4.8"
        );
        assert_eq!(default.reference_models.len(), 2);
        let parsed: MoaConfig = toml::from_str(
            "[presets.review]\nreference_models = [{provider = \"a\", model = \"x\"}]\n\
             aggregator = {provider = \"b\", model = \"y\"}\n",
        )
        .unwrap();
        assert_eq!(parsed.names(), ["default", "review"]);
        let review = parsed.preset("review").unwrap();
        assert!(review.enabled);
        assert_eq!(review.fanout, MoaFanout::default());
        assert_eq!(review.reference_temperature, None);
    }
}
