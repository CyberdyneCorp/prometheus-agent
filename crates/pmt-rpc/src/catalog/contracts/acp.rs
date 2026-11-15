//! What the ACP adapter needs of the daemon (spec: acp-adapter): the
//! session's editor policy (`session.editor`: editor MCP servers), its
//! settings and their changes (`session.settings`, `session.configure`,
//! announced to every client as a `session.settings` session event), and
//! the server requests that reach the editor itself (`editor.fs.read`,
//! `editor.fs.write`, `editor.terminal.run`), sent only to drivers that
//! advertised the matching client feature.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Method, ServerRequest, SessionRef};

/// The client feature of an ACP connection whose editor reads and writes
/// files for the agent (`fs.readTextFile` and `fs.writeTextFile`).
pub const EDITOR_FS: &str = "editor_fs";

/// The client feature of an ACP connection whose editor runs terminal
/// commands for the agent (`terminal`).
pub const EDITOR_TERMINAL: &str = "editor_terminal";

/// The `session.event` type announcing a change of the session's
/// settings (model, provider, effort, edit mode, cwd) by any surface:
/// `payload` is the new [`SessionSettings`].
pub const SESSION_SETTINGS: &str = "session.settings";

/// One `name=value` pair of an editor MCP server (an environment
/// variable or an HTTP header).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameValue {
    /// The name.
    pub name: String,
    /// The value.
    pub value: String,
}

/// An MCP server the editor asks the session to use (ACP `mcpServers`):
/// a local command (`stdio`) or a remote endpoint (`http`, `sse`). The
/// daemon runs it under the session's capability grants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorMcpServer {
    /// The server's name (its tools are `mcp_<name>_<tool>`).
    pub name: String,
    /// `stdio`, `http` or `sse`.
    pub transport: String,
    /// The program (`stdio`).
    #[serde(default)]
    pub command: Option<String>,
    /// Its arguments (`stdio`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Its environment (`stdio`).
    #[serde(default)]
    pub env: Vec<NameValue>,
    /// The endpoint (`http`, `sse`).
    #[serde(default)]
    pub url: Option<String>,
    /// Request headers (`http`, `sse`).
    #[serde(default)]
    pub headers: Vec<NameValue>,
}

/// `session.editor` params: the editor MCP servers of a session an
/// editor loaded, resumed or forked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorBindParams {
    /// The session.
    pub session_id: String,
    /// The servers: a non-empty list replaces the session's editor
    /// servers from its next turn, an empty one keeps them.
    #[serde(default)]
    pub mcp_servers: Vec<EditorMcpServer>,
}

/// `session.editor` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorBound {
    /// The names of the session's editor servers now.
    pub servers: Vec<String>,
    /// Whether the list changed (the new tools apply from the next turn).
    pub changed: bool,
}

/// How file edits of an editor session are approved (ACP session modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditMode {
    /// Ask before every file edit.
    Default,
    /// Edits inside the session's workspace run without asking.
    AcceptEdits,
    /// Every edit of this session runs without asking.
    DontAsk,
}

impl EditMode {
    /// Every mode, in the order editors list them.
    pub const ALL: [Self; 3] = [Self::Default, Self::AcceptEdits, Self::DontAsk];

    /// The wire name (the ACP mode id).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::DontAsk => "dont_ask",
        }
    }

    /// The mode of wire name `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == id)
    }
}

/// A session's settings as clients show and change them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    /// The session.
    pub session_id: String,
    /// The provider of the model the next turn uses.
    pub provider: String,
    /// That model (`<provider>/<model>` names it fully).
    pub model: String,
    /// The models a client may switch to (`<provider>/<model>`).
    #[serde(default)]
    pub models: Vec<String>,
    /// The reasoning effort (`none` when the model has none).
    pub reasoning_effort: String,
    /// The efforts the model accepts.
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    /// How file edits are approved (`null`: the session has no edit
    /// policy, it is not an editor session).
    pub edit_mode: Option<EditMode>,
    /// The working directory.
    pub cwd: String,
    /// Whether a turn runs.
    pub busy: bool,
    /// The session's title, once it has one.
    #[serde(default)]
    pub title: Option<String>,
}

/// `session.configure` params: the settings to change (absent ones keep).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureParams {
    /// The session.
    pub session_id: String,
    /// The edit mode; refused while a turn runs.
    #[serde(default)]
    pub edit_mode: Option<EditMode>,
    /// `<provider>/<model>`; while a turn runs it applies from the next.
    #[serde(default)]
    pub model: Option<String>,
    /// A reasoning effort the model accepts.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// A new working directory; refused while a turn runs.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `session.configure` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configured {
    /// The settings now (a deferred model shows once it applied).
    pub settings: SessionSettings,
    /// The model change waits for the running turn to end.
    pub next_turn: bool,
}

/// `editor.fs.read` params: a text file as the editor has it (unsaved
/// changes included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorReadParams {
    /// The session.
    pub session_id: String,
    /// The absolute path, inside the session's working directory.
    pub path: String,
    /// Milliseconds until the request settles unanswered.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// `editor.fs.write` params: the new content of a text file, written
/// through the editor (into an open buffer when there is one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorWriteParams {
    /// The session.
    pub session_id: String,
    /// The absolute path, inside the session's working directory.
    pub path: String,
    /// The whole new content.
    pub content: String,
    /// Milliseconds until the request settles unanswered.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// The answer to `editor.fs.read` and `editor.fs.write`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorFsResult {
    /// The file's content (`editor.fs.read`).
    #[serde(default)]
    pub content: Option<String>,
    /// The editor's error, returned to the model as the tool's.
    #[serde(default)]
    pub error: Option<String>,
}

/// `editor.terminal.run` params: a command the editor runs in its own
/// terminal panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorTerminalParams {
    /// The session.
    pub session_id: String,
    /// The shell command line.
    pub command: String,
    /// Its working directory.
    pub cwd: String,
    /// The most output bytes the editor keeps (the tool's output budget).
    pub output_byte_limit: u64,
    /// Milliseconds until the request settles unanswered.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// The answer to `editor.terminal.run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorTerminalResult {
    /// What the command printed (the editor's retained tail).
    #[serde(default)]
    pub output: String,
    /// The editor dropped output beyond the limit.
    #[serde(default)]
    pub truncated: bool,
    /// The exit status, when it exited.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// The signal that ended it, when one did.
    #[serde(default)]
    pub signal: Option<String>,
    /// The editor could not run it.
    #[serde(default)]
    pub error: Option<String>,
}

method!(/// `session.editor`: binds an editor's MCP servers to a session.
    SessionEditor = "session.editor" (EditorBindParams) -> EditorBound);
method!(/// `session.settings`: the session's model, effort, edit mode and cwd.
    SessionSettingsCall = "session.settings" (SessionRef) -> SessionSettings);
method!(/// `session.configure`: changes the session's settings.
    SessionConfigure = "session.configure" (ConfigureParams) -> Configured);
server_request!(/// `editor.fs.read`.
    EditorRead = "editor.fs.read" (EditorReadParams) -> EditorFsResult);
server_request!(/// `editor.fs.write`.
    EditorWrite = "editor.fs.write" (EditorWriteParams) -> EditorFsResult);
server_request!(/// `editor.terminal.run`.
    EditorTerminal = "editor.terminal.run" (EditorTerminalParams) -> EditorTerminalResult);

#[cfg(test)]
mod tests {
    use super::EditMode;

    #[test]
    fn every_edit_mode_parses_from_its_wire_name() {
        for mode in EditMode::ALL {
            assert_eq!(EditMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(EditMode::parse("yolo"), None);
    }
}
