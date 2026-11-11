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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct UiConfig {
    /// Interactive interface when stdin and stdout are terminals: the
    /// full-screen `tui` or the `line` CLI (`--tui`/`--line` override it).
    pub interface: Interface,
    /// Show the status bar (`/statusbar`, `/sb`); in the line interface a
    /// single bottom status line on a terminal.
    pub status_bar: bool,
    /// Show the battery level as the first status item (`/battery`);
    /// omitted on machines without a battery.
    pub status_bar_battery: bool,
    /// The composer of the TUI.
    pub composer: ComposerConfig,
    /// The full-screen terminal UI.
    pub tui: TuiConfig,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            interface: Interface::default(),
            status_bar: true,
            status_bar_battery: false,
            composer: ComposerConfig::default(),
            tui: TuiConfig::default(),
        }
    }
}

/// The `[ui.composer]` section (spec: tui, Large pastes collapse into
/// chips).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ComposerConfig {
    /// A paste of at least this many lines collapses into a chip; `0`
    /// disables the line threshold.
    pub paste_collapse_lines: u32,
    /// A paste of at least this many characters collapses into a chip;
    /// `0` disables the character threshold.
    pub paste_collapse_chars: u32,
}

impl Default for ComposerConfig {
    fn default() -> Self {
        Self {
            paste_collapse_lines: 5,
            paste_collapse_chars: 2000,
        }
    }
}

/// The `[ui.tui]` section (spec: tui, Status line and accessibility;
/// Configurable key bindings; Busy indicator styles; Auto-resume of the
/// last TUI session).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Colour theme: `auto` (from `COLORFGBG`, else the terminal's answer
    /// to a background-colour query, else dark), `dark` or `light`;
    /// `NO_COLOR` turns colours off whatever it says.
    pub theme: TuiTheme,
    /// No spinners or animation: a static busy glyph.
    pub reduced_motion: bool,
    /// The glyphs of the busy indicator (`/indicator`).
    pub busy_indicator: BusyIndicator,
    /// `recent`: a TUI launched without `--resume`, `--continue` or a
    /// prompt re-attaches to the session this profile last used in the
    /// TUI from the same working directory.
    pub auto_resume: AutoResume,
    /// Key bindings: action name to key chord (`Ctrl+P`, `Alt+Enter`,
    /// `PageUp`, `F2`); `/keys` lists the effective bindings.
    pub keys: TuiKeys,
}

/// Busy indicator styles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusyIndicator {
    /// Small faces.
    #[default]
    Kaomoji,
    /// Moon phases.
    Emoji,
    /// Rotating quarter circles.
    Unicode,
    /// A rotating bar.
    Ascii,
}

/// What a TUI opens when nothing names a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutoResume {
    /// A new session.
    #[default]
    Off,
    /// The session last used in the TUI from this directory.
    Recent,
}

/// TUI colour themes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TuiTheme {
    /// Picked for the terminal's background (dark when unknown).
    #[default]
    Auto,
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
}

/// A key chord: `[Ctrl+][Alt+][Shift+]<key>` (spec: tui, Configurable key
/// bindings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("x-key-chord" = true))]
pub struct KeyChord(pub String);

/// `[ui.tui.keys]`: one chord per action; an unset action keeps its
/// default, an unknown action name is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TuiKeys {
    /// Send the composer (default `Enter`).
    pub submit: Option<KeyChord>,
    /// Insert a line break (default `Alt+Enter`; `Shift+Enter` always
    /// does on terminals that report it).
    pub newline: Option<KeyChord>,
    /// Stop the running turn, else clear the composer (default `Ctrl+C`).
    pub interrupt: Option<KeyChord>,
    /// Leave the TUI when the composer is empty (default `Ctrl+D`).
    pub quit: Option<KeyChord>,
    /// Scroll the transcript one line up (default `Ctrl+Up`).
    pub scroll_up: Option<KeyChord>,
    /// Scroll the transcript one line down (default `Ctrl+Down`).
    pub scroll_down: Option<KeyChord>,
    /// Scroll the transcript one page up (default `PageUp`).
    pub page_up: Option<KeyChord>,
    /// Scroll the transcript one page down (default `PageDown`).
    pub page_down: Option<KeyChord>,
    /// The previous history entry (default `Up`).
    pub history_prev: Option<KeyChord>,
    /// The next history entry (default `Down`).
    pub history_next: Option<KeyChord>,
    /// Complete the `/command` or `@path` under the cursor (default `Tab`).
    pub complete: Option<KeyChord>,
    /// Move to the start of the line (default `Ctrl+A`).
    pub line_start: Option<KeyChord>,
    /// Move to the end of the line (default `Ctrl+E`).
    pub line_end: Option<KeyChord>,
    /// Clear the composer (default `Ctrl+U`).
    pub clear_input: Option<KeyChord>,
    /// Open the command palette (default `Ctrl+P`).
    pub palette: Option<KeyChord>,
    /// Open the live session switcher (default `Ctrl+X`).
    pub sessions: Option<KeyChord>,
    /// Attach the clipboard's image (default `Ctrl+V`).
    pub paste_image: Option<KeyChord>,
    /// Expand the paste chip before the cursor into its text (default
    /// `Ctrl+O`).
    pub expand_paste: Option<KeyChord>,
    /// Move to the startup banner's section headers (default `Ctrl+B`).
    pub banner: Option<KeyChord>,
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
    /// The UI language: `en` or another bundled id (`af`, `ar`, `de`,
    /// `es`, `fr`, `ga`, `hu`, `it`, `ja`, `ko`, `pt`, `ru`, `tr`, `uk`,
    /// `zh`, `zh-hant`), a language a profile overlay
    /// (`<profile_home>/locales/<id>.toml`) or an enabled language pack
    /// supplies. Localizes static UI text only, never what the model sees.
    pub language: String,
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
    /// Vim key bindings in the composer (the line editor is set up when
    /// the session starts).
    #[schemars(extend("x-apply" = "next_session"))]
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
            language: "en".to_owned(),
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

/// The `[busy]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct BusyConfig {
    /// What plain text typed while a turn runs does: `queue` runs it as
    /// the next turn, `steer` hands it to the running turn after its
    /// current tool step, `interrupt` stops the turn and then runs it.
    pub input_mode: BusyInputMode,
}

/// Busy input modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusyInputMode {
    /// Run the input as the next turn (first in, first out).
    #[default]
    Queue,
    /// Inject the input into the running turn.
    Steer,
    /// Stop the running turn, then run the input.
    Interrupt,
}

/// One `[quick_commands.<name>]` entry: a user slash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuickCommand {
    /// `alias` re-dispatches `target`; `exec` runs `command` in the local
    /// sandbox.
    #[serde(rename = "type")]
    pub kind: QuickCommandKind,
    /// The slash command an `alias` runs (with or without the leading
    /// `/`); the user's arguments are appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The shell command an `exec` entry runs, with every secret removed
    /// from its environment and its output redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// An `exec` command is killed after this many seconds.
    #[serde(default = "default_quick_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub timeout_secs: u64,
}

fn default_quick_timeout() -> u64 {
    30
}

/// Quick command kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuickCommandKind {
    /// Re-dispatch another slash command.
    Alias,
    /// Run a shell command.
    Exec,
}
