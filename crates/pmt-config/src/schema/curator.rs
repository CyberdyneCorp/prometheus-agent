//! The `[curator]` section and the skills-hub keys of `[skills]` (spec:
//! skills, Curator configuration and scheduling; Curator audit ledger,
//! single-edit rollback, purge and reports; Hub install pipeline and trust
//! policy; Skill signatures and provenance).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[curator]` section: the background pass that marks unused
/// agent-created skills stale and archives them (never deletes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
pub struct CuratorConfig {
    /// Run automatic passes.
    pub enabled: bool,
    /// Least time between two automatic passes.
    #[schemars(range(min = 1))]
    pub interval_secs: u64,
    /// The profile must have been idle this long before an automatic pass.
    #[schemars(range(min = 1))]
    pub min_idle_secs: u64,
    /// An active skill without activity this long becomes stale.
    #[schemars(range(min = 1))]
    pub stale_after_secs: u64,
    /// A skill without activity this long is archived.
    #[schemars(range(min = 1))]
    pub archive_after_secs: u64,
    /// Let a pass merge overlapping skills (a model review through the
    /// `auxiliary.curator` slot).
    pub consolidate: bool,
    /// Also manage ejected bundled copies.
    pub prune_builtins: bool,
    /// Backup archives kept under `skills/.backups/`.
    #[schemars(range(min = 1, max = 100))]
    pub backup_keep: u32,
    /// Archived skills older than this may be deleted by an explicit
    /// `prometheus curator purge` (0 = never; a pass never purges).
    pub archive_ttl_secs: u64,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 604_800,
            min_idle_secs: 7_200,
            stale_after_secs: 1_209_600,
            archive_after_secs: 2_592_000,
            consolidate: false,
            prune_builtins: false,
            backup_keep: 2,
            archive_ttl_secs: 0,
        }
    }
}

/// `skills.signature_policy`: what a hub install does with `SKILL.sig`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillSignaturePolicy {
    /// Signatures are not checked.
    Off,
    /// Unsigned skills install with a warning; an invalid signature is
    /// always refused.
    #[default]
    Warn,
    /// Unsigned and invalidly signed skills are refused (exit 77).
    Require,
}

/// The sources trusted by default (`skills.trusted_sources`).
pub const DEFAULT_TRUSTED_SOURCES: [&str; 2] = ["anthropics/skills", "openai/skills"];

/// The default `skills.ledger_max_bytes` (5 MiB).
pub const DEFAULT_LEDGER_MAX_BYTES: u64 = 5_242_880;
