//! The desktop UI toolset's way into an open window (spec: desktop-app,
//! Desktop UI toolset contract; Agent reactions; Terminal pane tools;
//! First-run onboarding chat; projects, Project tool for GUI sessions).
//!
//! Each window tool is one server-to-client request `ui.<tool>` carrying
//! the model's arguments as the tool defines them; only a driver whose
//! `rpc.hello` advertised the `desktop_ui` feature receives it, and its
//! answer is the tool's result.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Event, Method, ServerRequest};

/// The `rpc.hello` feature of a client that renders the desktop UI tools.
pub const DESKTOP_UI_FEATURE: &str = "desktop_ui";

/// `ui.<tool>` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiCall {
    /// The session whose turn called the tool.
    pub session_id: String,
    /// The tool's arguments (validated by the daemon first).
    pub arguments: Value,
    /// Milliseconds until the request settles unanswered (set by the
    /// daemon when it sends the request).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

macro_rules! ui_requests {
    ($($ty:ident = $name:literal: $doc:literal;)*) => {
        $(server_request!(#[doc = $doc] $ty = $name (UiCall) -> Value);)*
        /// Every `ui.*` request name, in tool order.
        pub const UI_REQUESTS: &[&str] = &[$($name),*];
    };
}

ui_requests! {
    UiReadTerminal = "ui.read_terminal": "`ui.read_terminal`: the focused terminal tab's text.";
    UiCloseTerminal = "ui.close_terminal": "`ui.close_terminal`: closes a background process's mirror tab.";
    UiDesktopPreview = "ui.desktop_preview": "`ui.desktop_preview`: opens, closes or reads a preview tab.";
    UiDrivePreview = "ui.drive_preview": "`ui.drive_preview`: acts on the preview page.";
    UiAnnotatePreview = "ui.annotate_preview": "`ui.annotate_preview`: marks preview elements.";
    UiReadWindowBelow = "ui.read_window_below": "`ui.read_window_below`: the OS window beneath the desktop window.";
    UiFocusPane = "ui.focus_pane": "`ui.focus_pane`: reveals a pane in the focused window.";
    UiGuiTour = "ui.gui_tour": "`ui.gui_tour`: tour targets, one highlighted step or a paged tour.";
    UiShowTip = "ui.show_tip": "`ui.show_tip`: one arrow bubble.";
    UiSetTheme = "ui.set_theme": "`ui.set_theme`: accent, mode, layout or a picker card.";
    UiSetupBeat = "ui.setup_beat": "`ui.setup_beat`: one onboarding card.";
}

/// One reaction on a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reaction {
    /// Who reacted (`agent`).
    pub by: String,
    /// The emoji.
    pub emoji: String,
}

/// `message.reaction` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageReaction {
    /// The session.
    pub session_id: String,
    /// The message.
    pub message_id: String,
    /// Its reactions after the change.
    pub reactions: Vec<Reaction>,
}

event!(/// `message.reaction`: a message's reactions changed.
    MessageReacted = "message.reaction" (MessageReaction));

/// `session.project` payload: a session moved to another project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionProject {
    /// The session.
    pub session_id: String,
    /// Its project.
    pub project_id: String,
    /// The project's slug.
    pub slug: String,
    /// The working directory the session moves to at its next tool call.
    pub cwd: String,
}

event!(/// `session.project`: the `project` tool moved a session.
    SessionProjectChanged = "session.project" (SessionProject));

/// `process.log` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessRef {
    /// The session.
    pub session_id: String,
    /// The background process.
    pub process_id: String,
}

/// One background process of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessEntry {
    /// Its id (`proc_…`).
    pub process_id: String,
    /// Its command line.
    pub command: String,
    /// `running`, `exited`...
    pub state: String,
}

/// `process.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessList {
    /// The session's live background processes.
    pub processes: Vec<ProcessEntry>,
}

/// `process.log` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessLog {
    /// The process.
    pub process_id: String,
    /// Its state.
    pub state: String,
    /// Its buffered output (redacted, shaped like the `process` tool's).
    pub output: String,
}

method!(/// `process.list`: a session's background processes (the desktop
    /// mirrors each in a read-only terminal tab).
    ProcessListCall = "process.list" (super::SessionRef) -> ProcessList);
method!(/// `process.log`: a background process's buffered output; reading it
    /// never changes the process or its quiet-wait timer.
    ProcessLogCall = "process.log" (ProcessRef) -> ProcessLog);

/// `onboarding.ensure` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnboardingChat {
    /// The profile's onboarding chat.
    pub session_id: String,
    /// Whether this call created it.
    pub created: bool,
}

method!(/// `onboarding.ensure`: the profile's first-run onboarding chat (the
    /// only session with the `onboarding` toolset), created when missing.
    OnboardingEnsure = "onboarding.ensure" (super::ProfileParams) -> OnboardingChat);
