//! Contracts of the experience library (spec: experience-library,
//! Library CLI): the `experience.*` methods the dashboard and the desktop
//! use. Each does what the matching `prometheus experience` subcommand
//! does.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// Params of every `experience.*` method; each uses the fields its
/// command takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// `experience.list`: only this domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// `experience.list`: also rolled-back entries.
    #[serde(default)]
    pub all: bool,
    /// The entry (`experience.show`, `experience.pin`,
    /// `experience.forget`, `experience.restore`).
    #[serde(default)]
    pub entry_id: Option<String>,
    /// `experience.pin`: `false` unpins (default `true`).
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// Result of the methods that act on one entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceReply {
    /// What happened, in one line.
    pub message: String,
    /// The entry's `entry.toml` as JSON (`experience.show` adds its
    /// `EXPERIENCE.md` as `document`).
    #[serde(default)]
    pub data: Value,
}

/// `experience.list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceEntries {
    /// Each entry's `entry.toml` as JSON: domains sorted, newest first.
    pub entries: Vec<Value>,
}

/// One domain of `experience.domains`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceDomain {
    /// The domain name.
    pub name: String,
    /// Its entries (any status).
    pub entries: usize,
}

/// `experience.domains` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceDomains {
    /// Sorted by name.
    pub domains: Vec<ExperienceDomain>,
}

method!(/// `experience.list`: the library's entries.
    ExperienceList = "experience.list" (ExperienceParams) -> ExperienceEntries);
method!(/// `experience.show`: one entry with its `EXPERIENCE.md`.
    ExperienceShow = "experience.show" (ExperienceParams) -> ExperienceReply);
method!(/// `experience.domains`: the domains with their entry counts.
    ExperienceDomainsList = "experience.domains" (ExperienceParams) -> ExperienceDomains);
method!(/// `experience.pin`: pins (or, with `pinned = false`, unpins) an entry.
    ExperiencePin = "experience.pin" (ExperienceParams) -> ExperienceReply);
method!(/// `experience.forget`: removes an entry (its harness version stays).
    ExperienceForget = "experience.forget" (ExperienceParams) -> ExperienceReply);
method!(/// `experience.disable`: turns an entry off (hidden until restored).
    ExperienceDisable = "experience.disable" (ExperienceParams) -> ExperienceReply);
method!(/// `experience.restore`: sets a rolled-back or disabled entry back to active.
    ExperienceRestore = "experience.restore" (ExperienceParams) -> ExperienceReply);
