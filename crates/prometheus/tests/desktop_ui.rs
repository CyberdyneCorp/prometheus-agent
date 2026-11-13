//! The desktop UI toolset end to end (spec: desktop-app, Desktop UI
//! toolset contract and the requirements after it; projects, Project tool
//! for GUI sessions): the page in the desktop browser harness against the
//! real daemon, with the mock model calling the tools, driving
//! `web/e2e/desktop-ui.spec.ts` and `web/e2e/desktop-onboarding.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use common::dashboard::playwright;
use common::desktop::{Setup, start_with};
use common::{append_config, mock_home, ok, run};
use pmt_desktop::below::Screen;
use pmt_desktop::hud::{Bounds, Candidate};
use pmt_desktop::platform::Facts;

const SCRIPT: &str = r##"
[[rule]]
user = "open the form"
[[rule.tool_calls]]
name = "desktop_preview"
arguments = { action = "open", url = "http://preview.test/form.html", label = "form" }

[[rule]]
user = "open the form"
tool_result = "\"opened\""
text = "the form is open"

[[rule]]
user = "list the elements"
[[rule.tool_calls]]
name = "drive_preview"
arguments = { action = "elements" }

[[rule]]
user = "list the elements"
tool_result = "\"ref\":\"e3\""
text = "listed"

[[rule]]
user = "mark two fields"
[[rule.tool_calls]]
name = "annotate_preview"
arguments = { action = "add", ref = "e1", label = "name" }

[[rule]]
user = "mark two fields"
tool_result = "\"marks\":1"
[[rule.tool_calls]]
name = "annotate_preview"
arguments = { action = "add", selector = "#pw" }

[[rule]]
user = "mark two fields"
tool_result = "\"marks\":2"
text = "marked"

[[rule]]
user = "type the password"
[[rule.tool_calls]]
name = "drive_preview"
arguments = { action = "type", selector = "#pw", text = "hunter2" }

[[rule]]
user = "type the password"
tool_result = "\"action\":\"type\""
text = "typed"

[[rule]]
user = "follow the link"
[[rule.tool_calls]]
name = "drive_preview"
arguments = { action = "click", ref = "e3" }

[[rule]]
user = "follow the link"
tool_result = "\"navigated\":true"
text = "on page two"

[[rule]]
user = "click the old ref"
[[rule.tool_calls]]
name = "drive_preview"
arguments = { action = "click", ref = "e1" }

[[rule]]
user = "click the old ref"
tool_result = "is stale"
text = "asked for new refs"

[[rule]]
user = "point at the heading"
[[rule.tool_calls]]
name = "gui_tour"
arguments = { action = "show", surface = "preview", selector = "h1", title = "This page" }

[[rule]]
user = "point at the heading"
tool_result = "\"marks\":1"
text = "pointed"

[[rule]]
user = "start the server"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "echo serving on 5173; sleep 600", background = true }

[[rule]]
user = "read the terminal"
[[rule.tool_calls]]
name = "read_terminal"
arguments = {}

[[rule]]
user = "read the terminal"
tool_result = ["serving on 5173", "viewport_rows"]
text = "the server is serving"

[[rule]]
user = "close the mirror"
[[rule.tool_calls]]
name = "close_terminal"
arguments = { process_id = "proc_" }

[[rule]]
user = "is the server alive"
[[rule.tool_calls]]
name = "process"
arguments = { action = "poll", process_id = "proc_" }

[[rule]]
user = "is the server alive"
tool_result = "\"state\":\"running\""
text = "still running"

[[rule]]
user = "what is below"
[[rule.tool_calls]]
name = "read_window_below"
arguments = {}

[[rule]]
user = "what is below"
tool_result = ["\"app\":\"Safari\"", "\"title\":\"\"", "Screen Recording"]
text = "Safari is below; its title needs Screen Recording"

[[rule]]
user = "use the cockpit layout"
[[rule.tool_calls]]
name = "set_theme"
arguments = { layout = "cockpit", accent = "violet" }

[[rule]]
user = "use the cockpit layout"
tool_result = "unknown layout 'cockpit'"
text = "no such layout"

[[rule]]
user = "go violet and dark"
[[rule.tool_calls]]
name = "set_theme"
arguments = { accent = "violet", mode = "dark" }

[[rule]]
user = "show me the files"
[[rule.tool_calls]]
name = "focus_pane"
arguments = { pane = "files" }

[[rule]]
user = "give me a tip"
[[rule.tool_calls]]
name = "show_tip"
arguments = { text = "Type your question here", selector = "form.composer", title = "Ask" }

[[rule]]
user = "take the tour"
[[rule.tool_calls]]
name = "gui_tour"
arguments = { action = "start", preset = "quick" }

[[rule]]
user = "take the tour"
tool_result = "\"completed\":true"
text = "tour done"

[[rule]]
user = "work on web"
[[rule.tool_calls]]
name = "project"
arguments = { action = "switch", project = "web" }

[[rule]]
user = "work on web"
tool_result = "from your next tool call"
text = "switched to web"

[[rule]]
user = "react to my first message"
[[rule.tool_calls]]
name = "react_to_message"
arguments = { emoji = "😂", messages_back = 1 }

[[rule]]
repeat = true
text = "ok"
"##;

struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// A macOS screen: this app's window over an untitled Safari window (no
/// Screen Recording permission).
fn macos_screen() -> Setup {
    let window = Bounds {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    };
    let candidate = |app: &str, title: &str, pid: u32, id: &str| Candidate {
        app: app.to_owned(),
        title: title.to_owned(),
        pid,
        layer: 0,
        id: id.to_owned(),
        bounds: window,
    };
    Setup {
        facts: Facts {
            os: "macos".to_owned(),
            wayland: false,
            sni_host: Some(true),
        },
        screen: Some(Screen {
            windows: vec![
                candidate("Prometheus", "Prometheus", std::process::id(), "11"),
                candidate("Safari", "", std::process::id() + 1, "42"),
            ],
            window,
        }),
        ..Setup::default()
    }
}

/// Scenario: Closing a mirror tab. Scenario: Stale ref. Scenario: Typing a
/// password. Scenario: Navigation clears marks. Scenario: Missing macOS
/// permission. Scenario: Unknown layout. Scenario: Switching project.
#[test]
fn the_agent_drives_the_window_through_the_desktop_ui_tools() {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), SCRIPT).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let code = home.path().canonicalize().unwrap().join("code");
    for name in ["api", "web"] {
        std::fs::create_dir_all(code.join(name)).unwrap();
        ok(&run(
            home.path(),
            &[
                "projects",
                "create",
                name,
                code.join(name).to_str().unwrap(),
            ],
        ));
    }
    let _stop = Stop(home.path());
    let harness = start_with(home.path(), Setup::default());
    let mac = start_with(home.path(), macos_screen());
    playwright(
        home.path(),
        "desktop-ui.spec.ts",
        &[
            ("PMT_DESKTOP_HARNESS", harness.url.as_str()),
            ("PMT_DESKTOP_HARNESS_MAC", mac.url.as_str()),
        ],
    );
}

const ONBOARDING: &str = r#"
[[rule]]
user = "Hi! Please help me set up Prometheus."
[[rule.tool_calls]]
name = "setup_beat"
arguments = { beat = "name", question = "What should I call you?" }

[[rule]]
user = "Hi! Please help me set up Prometheus."
tool_result = "Leo"
[[rule.tool_calls]]
name = "setup_beat"
arguments = { beat = "theme" }

[[rule]]
user = "Hi! Please help me set up Prometheus."
tool_result = "\"answer\":\"dark\""
text = "Welcome, Leo"

[[rule]]
user = "forge a setup card"
[[rule.tool_calls]]
name = "setup_beat"
arguments = { beat = "layout" }

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Regular chats lack the tool (desktop-app, First-run
/// onboarding chat, in the window).
#[test]
fn the_first_launch_opens_the_onboarding_chat() {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), ONBOARDING).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let _stop = Stop(home.path());
    let harness = start_with(
        home.path(),
        Setup {
            first_run: true,
            ..Setup::default()
        },
    );
    playwright(
        home.path(),
        "desktop-onboarding.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
    let prefs = std::fs::read_to_string(home.path().join("ui-prefs.json")).unwrap();
    assert!(
        prefs.contains(r#""onboarding": "skipped""#) || prefs.contains(r#""onboarding":"skipped""#),
        "{prefs}"
    );
}
