//! Interactive surface sections (spec: cli).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[cli]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct CliConfig {
    /// Ask `once / always / cancel` before `/clear`, `/new`, `/undo` and
    /// `/rewind`.
    pub confirm_destructive: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            confirm_destructive: true,
        }
    }
}

/// The `[ui]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct UiConfig {
    /// Interactive interface on a TTY (M1 ships `line`; `tui` falls back
    /// to it until the TUI lands).
    pub interface: Interface,
}

/// Interactive interfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Interface {
    /// The full-screen terminal UI.
    #[default]
    Tui,
    /// The line-mode CLI.
    Line,
}

/// The `[display]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
pub struct DisplayConfig {
    /// Theme: a built-in (`default`, `mono`, `daylight`, `slate`) or a
    /// file in `<profile>/themes/`.
    pub theme: String,
    /// Ring the terminal bell when a turn completes.
    pub bell_on_complete: bool,
    /// Ring the terminal bell when an approval or clarify prompt opens.
    pub bell_on_prompt: bool,
    /// Show the session cost in the status line.
    pub show_cost: bool,
    /// Show model reasoning.
    pub show_reasoning: bool,
    /// Show reasoning in full instead of clamped.
    pub reasoning_full: bool,
    /// How the final response is printed.
    pub final_response_markdown: MarkdownMode,
    /// Show file-edit diffs inline.
    pub inline_diffs: bool,
    /// Vim key bindings in the composer.
    pub vim_mode: bool,
    /// Compact layout.
    pub compact: bool,
    /// One line with duration, tool count and tokens after each turn.
    pub turn_summary: bool,
    /// Show assistant text between tool calls.
    pub interim_messages: bool,
    /// Prefix messages with timestamps.
    pub timestamps: bool,
    /// `strftime` format of message timestamps.
    pub timestamp_format: String,
    /// Recap printed when a session is resumed.
    pub resume_display: ResumeDisplay,
    /// User/assistant exchanges shown in the recap.
    pub resume_exchanges: u32,
    /// User message characters shown in the recap.
    pub resume_max_user_chars: u32,
    /// Characters of non-final assistant messages shown in the recap.
    pub resume_max_assistant_chars: u32,
    /// Lines of non-final assistant messages shown in the recap.
    pub resume_max_assistant_lines: u32,
    /// Tool activity shown while a turn runs.
    pub tool_progress: ToolProgress,
    /// Tool preview length in characters; `0` means no limit.
    pub tool_preview_length: u32,
    /// How consecutive tool calls are grouped.
    pub tool_progress_grouping: ToolProgressGrouping,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_owned(),
            bell_on_complete: false,
            bell_on_prompt: false,
            show_cost: false,
            show_reasoning: false,
            reasoning_full: false,
            final_response_markdown: MarkdownMode::Render,
            inline_diffs: true,
            vim_mode: false,
            compact: false,
            turn_summary: true,
            interim_messages: true,
            timestamps: false,
            timestamp_format: "%H:%M".to_owned(),
            resume_display: ResumeDisplay::Full,
            resume_exchanges: 10,
            resume_max_user_chars: 300,
            resume_max_assistant_chars: 200,
            resume_max_assistant_lines: 3,
            tool_progress: ToolProgress::All,
            tool_preview_length: 0,
            tool_progress_grouping: ToolProgressGrouping::Accumulate,
        }
    }
}

/// Final response rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownMode {
    /// Render Markdown.
    Render,
    /// Print the raw text.
    Raw,
}

/// Resume recap modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDisplay {
    /// Recent exchanges.
    Full,
    /// Title, message count and last activity only.
    Minimal,
    /// No recap.
    Off,
}

/// Tool progress modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgress {
    /// Nothing.
    Off,
    /// A line only when the tool changes.
    New,
    /// A line per call.
    All,
    /// Arguments and result previews too.
    Verbose,
}

/// Tool progress grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressGrouping {
    /// Consecutive calls share one updating line.
    Accumulate,
    /// One line per call.
    Separate,
}
