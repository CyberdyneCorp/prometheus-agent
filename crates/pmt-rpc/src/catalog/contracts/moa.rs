//! Contracts of Mixture of Agents preset management (spec:
//! mixture-of-agents, Preset management commands): the dashboard and
//! desktop preset editors list, write and delete `[moa.presets.<name>]`.
//! An edit applies to model calls of sessions that start after it; a call
//! in flight is never altered.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// One preset slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaSlotInfo {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// `none`...`ultra`, when set.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// One preset as editors show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetInfo {
    /// The preset name (`/model <name>`, `moa/<name>`).
    pub name: String,
    /// It is `moa.default_preset` (cannot be deleted).
    pub default: bool,
    /// The built-in `default`, not redefined in `config.toml`.
    pub builtin: bool,
    /// `false`: the aggregator runs alone.
    pub enabled: bool,
    /// The model that acts with the tools and is billed.
    pub aggregator: MoaSlotInfo,
    /// The advisors.
    pub references: Vec<MoaSlotInfo>,
    /// When the advisors run (`user_turn`, `per_iteration`, `every_n:N`).
    pub fanout: String,
    /// Temperature sent to the references.
    #[serde(default)]
    pub reference_temperature: Option<f64>,
    /// Temperature sent to the aggregator.
    #[serde(default)]
    pub aggregator_temperature: Option<f64>,
    /// `Aggregator is on <provider>; ...` when it bills another provider
    /// than `model.provider`.
    #[serde(default)]
    pub billing_notice: Option<String>,
}

/// `moa.presets.list` (and `put`, `delete`) result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetList {
    /// `moa.default_preset`.
    pub default_preset: String,
    /// Every preset, sorted by name.
    pub presets: Vec<MoaPresetInfo>,
}

/// `moa.presets.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetsListParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// A preset to write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetInput {
    /// The advisors (at least one).
    pub references: Vec<MoaSlotInfo>,
    /// The aggregator (never `moa`).
    pub aggregator: MoaSlotInfo,
    /// Temperature sent to the references (0.0-2.0).
    #[serde(default)]
    pub reference_temperature: Option<f64>,
    /// Temperature sent to the aggregator (0.0-2.0).
    #[serde(default)]
    pub aggregator_temperature: Option<f64>,
    /// The cadence (default `user_turn`).
    #[serde(default)]
    pub fanout: Option<String>,
    /// Whether the references run (default `true`).
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `moa.presets.put` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetsPutParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The preset name.
    pub name: String,
    /// The preset.
    pub preset: MoaPresetInput,
}

/// `moa.presets.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoaPresetsDeleteParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The preset name (never `moa.default_preset`).
    pub name: String,
}

method!(/// `moa.presets.list`: the profile's Mixture of Agents presets.
    MoaPresetsList = "moa.presets.list" (MoaPresetsListParams) -> MoaPresetList);
method!(/// `moa.presets.put`: creates or replaces a preset; answers the list.
    MoaPresetsPut = "moa.presets.put" (MoaPresetsPutParams) -> MoaPresetList);
method!(/// `moa.presets.delete`: deletes a configured preset other than the
    /// default one; answers the list.
    MoaPresetsDelete = "moa.presets.delete" (MoaPresetsDeleteParams) -> MoaPresetList);
