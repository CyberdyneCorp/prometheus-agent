//! A recording's initial state (`state.json`; spec: replay-and-evaluation,
//! Session recordings): what the recorded session started from.

use serde::{Deserialize, Serialize};

/// `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The recorded session's surface (`cli`, `cron`...).
    pub source: String,
    /// Its source tag.
    pub source_tag: Option<String>,
    /// Its provider.
    pub provider: Option<String>,
    /// Its model.
    pub model: Option<String>,
    /// Its working directory.
    pub cwd: Option<String>,
    /// The profile's effective `config.toml` (secrets appear only as the
    /// names it references).
    pub config: String,
    /// The secret names the config references.
    pub secret_names: Vec<String>,
    /// The toolsets the session exposed.
    pub toolsets: Vec<String>,
    /// The tool schemas the session froze.
    pub tool_schemas: serde_json::Value,
    /// `sha256:<hex>` of the skills the prompt indexed and pinned.
    pub skills_sha256: Option<String>,
    /// `sha256:<hex>` of the memory snapshot in the prompt.
    pub memory_sha256: Option<String>,
    /// The build that recorded the session.
    pub harness_version: String,
    /// The mock script its mock routes played, when it used one.
    pub mock_script: Option<String>,
    /// Workspace file contents are in the recording.
    pub workspace_included: bool,
}
