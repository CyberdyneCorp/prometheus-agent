//! `cron.blueprints.list` and `cron.blueprints.fill` (spec: scheduled-jobs,
//! Automation blueprint catalog): the dashboard and desktop render a
//! blueprint's slots as a form and fill it without anyone typing cron
//! syntax.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `cron.blueprints.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintsListParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// One slot, as a form field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintSlot {
    /// The field name (`time`, `days`...).
    pub name: String,
    /// `time`, `enum`, `text` or `weekdays`.
    pub kind: String,
    /// The question asked for it.
    pub question: String,
    /// The options of an `enum`; the presets of `weekdays`.
    pub options: Vec<String>,
    /// An `enum` accepts only its options.
    pub strict: bool,
    /// The value used when none is given.
    pub default: Option<String>,
    /// Needs a value (given or default).
    pub required: bool,
}

/// One blueprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintInfo {
    /// Its key (`<plugin>:<key>`, `skill:<key>` outside the built-ins).
    pub key: String,
    /// Its title.
    pub title: String,
    /// What it does.
    pub description: String,
    /// Its category.
    pub category: String,
    /// `builtin`, `plugin` or `skill`.
    pub source: String,
    /// Its slots, in the order they are asked.
    pub slots: Vec<BlueprintSlot>,
}

/// `cron.blueprints.list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintList {
    /// The profile's catalog.
    pub blueprints: Vec<BlueprintInfo>,
}

/// `cron.blueprints.fill` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintFillParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The blueprint (key, prefix or near spelling).
    pub key: String,
    /// Slot values by name.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Save the job (after the caller showed the echo); otherwise only
    /// check and echo.
    #[serde(default)]
    pub create: bool,
}

/// `cron.blueprints.fill` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintFilled {
    /// `missing` (a slot needs a value), `ready` (echoed, not saved) or
    /// `created`.
    pub status: String,
    /// The blueprint's key.
    pub key: String,
    /// The slot that needs a value.
    pub missing: Option<String>,
    /// Its question.
    pub question: Option<String>,
    /// The canonical schedule (`cron 0 8 * * 1-5`).
    pub schedule: Option<String>,
    /// The schedule echo: the canonical form and the next instants.
    pub echo: Option<String>,
    /// The job's prompt.
    pub prompt: Option<String>,
    /// The created job's id.
    pub job_id: Option<String>,
}

method!(/// `cron.blueprints.list`: the profile's automation blueprints.
    BlueprintsListCall = "cron.blueprints.list" (BlueprintsListParams) -> BlueprintList);
method!(/// `cron.blueprints.fill`: checks slot values, echoes the schedule and
    /// creates the job when asked.
    BlueprintsFillCall = "cron.blueprints.fill" (BlueprintFillParams) -> BlueprintFilled);
