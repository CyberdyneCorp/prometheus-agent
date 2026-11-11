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
    /// Below this many free bytes on the store's volume an interactive
    /// start offers to turn on auto-prune (when `retention_secs = 0`).
    pub low_disk_bytes: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            terminal_continue: true,
            retention_secs: 0,
            auto_archive_secs: 604_800,
            max_resume_messages: 20_000,
            max_export_messages: 20_000,
            low_disk_bytes: 1_073_741_824,
        }
    }
}

/// The `[attachments]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct AttachmentsConfig {
    /// Where received attachments are kept.
    pub storage: AttachmentStorage,
}

/// Attachment storage locations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentStorage {
    /// The profile blob store only.
    #[default]
    Blobs,
    /// The blob store plus a copy under
    /// `<cwd>/.prometheus/attachments/<session_id>/` for tools to read by
    /// path.
    Workspace,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
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
    /// The external memory provider: `builtin` (none) or the id of an
    /// installed `memory-provider` plugin.
    pub provider: String,
    /// Time a provider's `prefetch` may take before the turn goes on
    /// without its recall.
    #[schemars(range(min = 1))]
    pub provider_prefetch_timeout_ms: u64,
    /// Abort a compaction when no provider confirms its pre-compress
    /// checkpoint.
    pub require_checkpoint: bool,
    /// Character bound of a provider's recall per turn.
    pub provider_recall_max_chars: u64,
    /// Spill oversized provider recall to a file under `cache/` (a
    /// preview and the path stay in the fence) instead of truncating it.
    pub provider_recall_spill: bool,
    /// Notices after a memory change commits: `off`, `on` or `verbose`.
    pub notifications: MemoryNotifications,
    /// The background memory review.
    pub review: MemoryReviewConfig,
    /// Each provider's own non-secret settings (`memory.providers.<id>.*`;
    /// its secrets live in the profile's secrets store).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// `memory.notifications`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryNotifications {
    /// No notice.
    Off,
    /// A generic line (`Memory updated`).
    #[default]
    On,
    /// The line plus a compact preview of the change.
    Verbose,
}

/// When a background review whose route is a managed local model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDefer {
    /// Queue it until the machine is idle.
    #[default]
    Auto,
    /// Run it at once.
    Never,
}

/// The `[memory.review]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryReviewConfig {
    /// Run automatic post-turn reviews (`/refine` works either way).
    pub enabled: bool,
    /// Replayed input tokens one review may send in total; unset is 75%
    /// of the review model's window (at most 600000, 120000 when the
    /// window is unknown); 0 or negative is unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<i64>,
    /// Tools added to the review's allow-list when the parent session has
    /// them.
    pub extra_tools: Vec<String>,
    /// `auto` queues reviews on a managed local model until the machine
    /// is idle; `never` runs them at once.
    pub defer: ReviewDefer,
    /// A deferred review older than this runs anyway.
    pub defer_max_age_secs: u64,
}

impl Default for MemoryReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_input_tokens: None,
            extra_tools: Vec::new(),
            defer: ReviewDefer::Auto,
            defer_max_age_secs: 1800,
        }
    }
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
            provider: "builtin".to_owned(),
            provider_prefetch_timeout_ms: 8000,
            require_checkpoint: false,
            provider_recall_max_chars: 100_000,
            provider_recall_spill: false,
            notifications: MemoryNotifications::On,
            review: MemoryReviewConfig::default(),
            providers: BTreeMap::new(),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RecallConfig {
    /// Archival entries recalled per turn.
    pub top_k: u32,
    /// Character cap of recalled content per turn.
    pub max_chars: u32,
    /// Embedding model (`provider/model` or a model on the main route's
    /// provider) for hybrid semantic recall; unset ranks lexically (BM25).
    pub embedding_model: Option<String>,
    /// Hybrid recall drops entries scoring below this.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub min_score: f64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            top_k: 5,
            max_chars: 1500,
            embedding_model: None,
            min_score: 0.35,
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
    /// Hub sources (`owner/repo`) whose skills install with trust
    /// `trusted` (`safe` and `caution` verdicts allowed).
    pub trusted_sources: Vec<String>,
    /// What a hub install does with a skill's `SKILL.sig`.
    pub signature_policy: super::curator::SkillSignaturePolicy,
    /// The signers whose valid `SKILL.sig` counts as verified: minisign
    /// key ids (`minisign:<KEYID>` or the bare 16 hex digits) or public
    /// keys, and Sigstore identities (`sigstore:<subject> via <issuer>`).
    /// A valid signature by anyone else is `untrusted`.
    pub trusted_signers: Vec<String>,
    /// The minisign secret key `skills publish` signs with (empty: none).
    pub signing_key: String,
    /// Size of the skill ledger before its oldest entries are compacted
    /// (0 = unbounded).
    pub ledger_max_bytes: u64,
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
            trusted_sources: super::curator::DEFAULT_TRUSTED_SOURCES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            signature_policy: super::curator::SkillSignaturePolicy::Warn,
            trusted_signers: Vec::new(),
            signing_key: String::new(),
            ledger_max_bytes: super::curator::DEFAULT_LEDGER_MAX_BYTES,
        }
    }
}
