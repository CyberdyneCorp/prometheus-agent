//! Session storage, snapshot, memory and skills sections (specs:
//! session-storage, configuration-and-profiles, memory, skills, cli).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[sessions]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct SessionsConfig {
    /// `-c` prefers the session last used from the same terminal.
    pub terminal_continue: bool,
    /// Prune ended sessions inactive for longer than this; `0` never prunes.
    pub retention_secs: u64,
    /// Archive lineage tips idle for this long.
    pub auto_archive_secs: u64,
    /// Refuse to resume a lineage with more messages; `0` disables.
    pub max_resume_messages: u64,
    /// Refuse in-memory exports with more messages.
    pub max_export_messages: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            terminal_continue: true,
            retention_secs: 0,
            auto_archive_secs: 604_800,
            max_resume_messages: 20_000,
            max_export_messages: 20_000,
        }
    }
}

/// The `[storage]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct StorageConfig {
    /// `state.db` journal mode; `delete` only for filesystems without WAL
    /// support.
    pub journal_mode: JournalMode,
}

/// SQLite journal modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JournalMode {
    /// Write-ahead log.
    #[default]
    Wal,
    /// Rollback journal (network filesystems).
    Delete,
}

/// The `[snapshots]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct SnapshotsConfig {
    /// Manual snapshots kept per profile.
    pub keep: u32,
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self { keep: 20 }
    }
}

/// The `[memory]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct MemoryConfig {
    /// The agent memory store.
    pub memory_enabled: bool,
    /// The user profile store.
    pub user_profile_enabled: bool,
    /// Character budget of core memory entries.
    pub memory_char_limit: u32,
    /// Character budget of core user-profile entries.
    pub user_char_limit: u32,
    /// Archival entries kept per target.
    pub archival_max_entries: u32,
    /// Approve every memory write.
    pub write_approval: bool,
    /// How agent memory changes are reviewed.
    pub review_mode: MemoryReviewMode,
    /// User turns without a memory call before a background review; `0`
    /// disables.
    pub nudge_interval: u32,
    /// Per-turn recall.
    pub recall: RecallConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_enabled: true,
            user_profile_enabled: true,
            memory_char_limit: 2200,
            user_char_limit: 1375,
            archival_max_entries: 10_000,
            write_approval: false,
            review_mode: MemoryReviewMode::Summary,
            nudge_interval: 10,
            recall: RecallConfig::default(),
        }
    }
}

/// Memory review modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReviewMode {
    /// One-line summary per session.
    Summary,
    /// No summary.
    Off,
    /// Stage every agent change as pending.
    Require,
}

/// The `[memory.recall]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RecallConfig {
    /// Archival entries recalled per turn.
    pub top_k: u32,
    /// Character cap of recalled content per turn.
    pub max_chars: u32,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            top_k: 5,
            max_chars: 1500,
        }
    }
}

/// The `[skills]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
pub struct SkillsConfig {
    /// Extra skill directories (`~` expanded; missing ones skipped).
    pub external_dirs: Vec<String>,
    /// Git roots whose project-local skills are trusted.
    pub trusted_project_dirs: Vec<String>,
    /// Scan for untrusted project skills and show a notice.
    pub project_discovery: bool,
    /// Disabled skill names.
    pub disabled: Vec<String>,
    /// Disabled skill names per platform or surface.
    pub platform_disabled: BTreeMap<String, Vec<String>>,
    /// Description length in the skills index.
    pub index_description_chars: u32,
    /// Substitute `${PROMETHEUS_SKILL_DIR}`-style tokens at load.
    pub template_vars: bool,
    /// Expand inline shell snippets at load.
    pub inline_shell: bool,
    /// Deadline of one inline shell snippet.
    pub inline_shell_timeout_ms: u64,
    /// Stage skill writes as pending for review.
    pub write_approval: bool,
    /// Tool-calling iterations before a skill-creation nudge.
    pub creation_nudge_interval: u32,
    /// Skills loaded into every new session.
    pub auto_load: Vec<String>,
    /// Show the bundled skills tier.
    pub include_bundled: bool,
    /// Non-secret per-skill settings: skill to key to value.
    pub config: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            external_dirs: Vec::new(),
            trusted_project_dirs: Vec::new(),
            project_discovery: true,
            disabled: Vec::new(),
            platform_disabled: BTreeMap::new(),
            index_description_chars: 60,
            template_vars: true,
            inline_shell: false,
            inline_shell_timeout_ms: 10_000,
            write_approval: false,
            creation_nudge_interval: 10,
            auto_load: Vec::new(),
            include_bundled: true,
            config: BTreeMap::new(),
        }
    }
}
