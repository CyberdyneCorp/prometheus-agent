//! The registry entries of the desktop UI toolset, the onboarding beat
//! and the `project` tool (spec: desktop-app, Desktop UI toolset contract
//! and the requirements after it; projects, Project tool for GUI
//! sessions).
//!
//! The turn loop executes them (`pmt_agent::tools::desktop`): the window
//! tools reach the session's open desktop window, `react_to_message` and
//! `project` change the session itself. Names the tools validate
//! themselves (panes, layouts, accents) are plain strings in the schemas,
//! so the model gets the tool's own error listing the allowed values.

use serde_json::{Value, json};

use crate::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};

/// The desktop UI toolset.
pub const DESKTOP_UI: &str = "desktop_ui";
/// The onboarding chat's toolset.
pub const ONBOARDING: &str = "onboarding";
/// The project toolset.
pub const PROJECT: &str = "project";

/// The members of `desktop_ui`, in order.
pub const DESKTOP_UI_TOOLS: &[&str] = &[
    "react_to_message",
    "read_terminal",
    "close_terminal",
    "desktop_preview",
    "drive_preview",
    "annotate_preview",
    "read_window_below",
    "focus_pane",
    "gui_tour",
    "show_tip",
    "set_theme",
];

/// Most terminal lines one `read_terminal` returns.
pub const TERMINAL_MAX_LINES: u64 = 2_000;
/// Most characters one `desktop_preview` read returns.
pub const PREVIEW_MAX_CHARS: u64 = 20_000;

/// The entries this build registers.
#[must_use]
pub fn tools() -> Vec<RegisteredTool> {
    let mut tools: Vec<RegisteredTool> = window_tools()
        .into_iter()
        .map(|(name, effect, description, parameters)| {
            let mut d = ToolDescriptor::new(name, DESKTOP_UI, description, parameters);
            d.effect = effect;
            d.idempotent = matches!(name, "read_terminal" | "read_window_below");
            // One window: its changes apply in call order.
            d.claims = Some(ResourceClaims::exclusive("desktop_window"));
            RegisteredTool::agent_loop(d)
        })
        .collect();
    tools.push(RegisteredTool::agent_loop(setup_beat()));
    tools.push(RegisteredTool::agent_loop(project()));
    tools
}

type Entry = (&'static str, Effect, &'static str, Value);

fn window_tools() -> Vec<Entry> {
    let mut entries = vec![
        (
            "react_to_message",
            Effect::None,
            "Set your single reaction on a user message of this conversation (default the \
             latest; messages_back = n reacts to the n-th earlier one). An empty emoji takes \
             your reaction back.",
            object(
                &json!({
                    "emoji": {"type": "string"},
                    "message_id": {"type": "string"},
                    "messages_back": {"type": "integer", "minimum": 0}
                }),
                &["emoji"],
            ),
        ),
        (
            "read_terminal",
            Effect::None,
            "Read the focused terminal tab beside the chat: the visible screen without \
             arguments, or scrollback lines from start_line (0 = oldest), at most 2000.",
            object(
                &json!({
                    "start_line": {"type": "integer", "minimum": 0},
                    "count": {"type": "integer", "minimum": 1, "maximum": TERMINAL_MAX_LINES}
                }),
                &[],
            ),
        ),
        (
            "close_terminal",
            Effect::None,
            "Close the read-only tab mirroring a background process; the process keeps \
             running and its output keeps buffering.",
            object(&json!({"process_id": {"type": "string"}}), &["process_id"]),
        ),
        (
            "desktop_preview",
            Effect::None,
            "Open a web page, a local dev server or a workspace file in the preview rail, \
             close the rail or one tab (label), or read a browser tab's visible text paged \
             by character offset (at most 20000 per read).",
            object(
                &json!({
                    "action": {"type": "string", "enum": ["open", "close", "read"]},
                    "url": {"type": "string"},
                    "label": {"type": "string"},
                    "start": {"type": "integer", "minimum": 0},
                    "count": {"type": "integer", "minimum": 1, "maximum": PREVIEW_MAX_CHARS}
                }),
                &["action"],
            ),
        ),
        drive_preview(),
        (
            "annotate_preview",
            Effect::None,
            "Draw lasting outlines on preview elements: add (ref or selector, optional short \
             label), hold (outline every visible element with its ref), remove one, clear all. \
             Navigation clears them.",
            object(
                &json!({
                    "action": {"type": "string", "enum": ["add", "hold", "remove", "clear"]},
                    "ref": {"type": "string"},
                    "selector": {"type": "string"},
                    "label": {"type": "string", "maxLength": 40}
                }),
                &["action"],
            ),
        ),
        (
            "read_window_below",
            Effect::None,
            "Metadata (app, title, bounds) of the OS window directly beneath the desktop \
             window. Never captures pixels.",
            object(&json!({}), &[]),
        ),
        (
            "focus_pane",
            Effect::None,
            "Reveal a pane in the focused window: chat, files, terminal, review or sessions.",
            object(&json!({"pane": {"type": "string"}}), &["pane"]),
        ),
    ];
    entries.extend(guidance_tools());
    entries
}

fn drive_preview() -> Entry {
    (
        "drive_preview",
        Effect::None,
        "Act on the preview page. `elements` lists interactive elements with stable refs; \
         click, hover, type (text, submit), scroll (amount or to), press (key; a printable \
         key on the page body needs allow_shortcut), strobe, back, forward and reload \
         answer with what changed (added, changed, removed, rebound). Typing into a \
         password field asks the user first.",
        object(
            &json!({
                "action": {"type": "string", "enum": ["elements", "click", "hover", "type", "scroll", "press", "strobe", "back", "forward", "reload"]},
                "ref": {"type": "string"},
                "selector": {"type": "string"},
                "text": {"type": "string"},
                "submit": {"type": "boolean"},
                "key": {"type": "string"},
                "allow_shortcut": {"type": "boolean"},
                "amount": {"type": "integer"},
                "to": {"type": "string", "enum": ["top", "bottom"]},
                "max": {"type": "integer", "minimum": 1},
                "full": {"type": "boolean"}
            }),
            &["action"],
        ),
    )
}

fn guidance_tools() -> Vec<Entry> {
    vec![
        (
            "gui_tour",
            Effect::None,
            "Guide the user through the app: list stable targets, show one highlighted \
             step, or start a paged tour (your steps, or the built-in quick/full tour) and \
             move it with next, prev and stop.",
            object(
                &json!({
                    "action": {"type": "string", "enum": ["targets", "show", "start", "next", "prev", "stop"]},
                    "surface": {"type": "string", "enum": ["app", "preview"]},
                    "selector": {"type": "string"},
                    "title": {"type": "string"},
                    "text": {"type": "string"},
                    "side": {"type": "string", "enum": ["top", "bottom", "left", "right"]},
                    "steps": {"type": "array", "items": {"type": "object"}},
                    "step_index": {"type": "integer", "minimum": 0},
                    "preset": {"type": "string", "enum": ["quick", "full"]}
                }),
                &["action"],
            ),
        ),
        (
            "show_tip",
            Effect::None,
            "Show one arrow bubble pointing at an element (replaces the previous tip; does \
             not wait for the user).",
            object(
                &json!({
                    "text": {"type": "string"},
                    "selector": {"type": "string"},
                    "title": {"type": "string"},
                    "side": {"type": "string", "enum": ["top", "bottom", "left", "right"]}
                }),
                &["text", "selector"],
            ),
        ),
        (
            "set_theme",
            Effect::None,
            "Change the app's look: accent (mono, green, cyan, violet, pink, red, orange, \
             theme or #rrggbb), mode (light, dark, system) and layout (a preset or saved \
             layout id), or with picker alone show a picker card and wait for the user's \
             choice. Never changes the conversation or any profile setting.",
            object(
                &json!({
                    "accent": {"type": "string"},
                    "mode": {"type": "string"},
                    "layout": {"type": "string"},
                    "picker": {"type": "string", "enum": ["accent", "layout"]}
                }),
                &[],
            ),
        ),
    ]
}

fn setup_beat() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "setup_beat",
        ONBOARDING,
        "Show the next first-run setup card and wait for the user's answer. Beats go in \
         order: name, theme, layout, connectors, plugins (suggest at most 5 catalog \
         plugins), tour, build. Returns the answer and the next beat.",
        object(
            &json!({
                "beat": {"type": "string", "enum": ["name", "theme", "layout", "connectors", "plugins", "tour", "build"]},
                "question": {"type": "string"},
                "value": {},
                "skip": {"type": "boolean"},
                "suggest": {"type": "array", "items": {"type": "string"}},
                "options": {"type": "array", "items": {"type": "string"}}
            }),
            &["beat"],
        ),
    );
    d.effect = Effect::None;
    d.claims = Some(ResourceClaims {
        interactive: true,
        ..ResourceClaims::exclusive("desktop_window")
    });
    d
}

fn project() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "project",
        PROJECT,
        "List this profile's projects (yours marked), create one (name, optional path; an \
         existing project owning the path is returned) or switch to one (project id or \
         slug). create and switch move this session's working directory to the project's \
         primary folder from your next tool call; nothing else changes.",
        object(
            &json!({
                "action": {"type": "string", "enum": ["list", "create", "switch"]},
                "name": {"type": "string"},
                "path": {"type": "string"},
                "project": {"type": "string"}
            }),
            &["action"],
        ),
    );
    d.effect = Effect::Workspace;
    d.claims = Some(ResourceClaims::exclusive("workspace"));
    d
}

fn object(properties: &Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}
