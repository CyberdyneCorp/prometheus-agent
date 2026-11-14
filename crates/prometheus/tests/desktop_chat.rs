//! The desktop chat end to end (spec: desktop-app, Chat transcript
//! rendering; Drafts are durable and never sent for the user; Composer
//! history and queue editing; Desktop status bar; Composer model picker;
//! Subagent route settings are drafts until applied): the page in the
//! desktop browser harness (`common/desktop.rs`) against the real daemon,
//! driving `web/e2e/desktop-chat.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use common::dashboard::playwright;
use common::{append_config, mock_home, ok, run};

/// What the mock model answers: a tool that writes an image, a slow turn
/// (a minute, so a daemon restart interrupts it) and `ok` otherwise.
const SCRIPT: &str = r#"
[[rule]]
user = "draw the chart"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "out/chart.svg", content = "<svg xmlns='http://www.w3.org/2000/svg' width='12' height='12'><rect width='12' height='12' fill='red'/></svg>" }

[[rule]]
tool_result = "chart.svg"
text = "Drew it"

[[rule]]
user_regex = '^slow$'
latency_ms = 60000
text = "slow done"
repeat = true

[[rule]]
repeat = true
text = "ok"
"#;

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// Scenario: Jump from the timeline. Scenario: Remote image. Scenario:
/// Session deleted elsewhere. Scenario: Edit a queued prompt. Scenario:
/// Lost turn blocks the queue. Scenario: Inspecting context use.
/// Scenario: Bypass is session-scoped. Scenario: Picker does not change
/// the default. Scenario: Favourite for a disconnected provider.
/// Scenario: Leaving without applying.
#[test]
fn desktop_chat_composer_queue_status_bar_and_model_picker() {
    let home = mock_home();
    let script = home.path().join("profiles/default/mock.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "model-a"],
    ));
    append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[model.aliases]\nb = \"mock/model-b\"\n",
    );
    // A restart interrupts the slow turn instead of waiting for it.
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon]\nrestart_drain_secs = 1\n",
    )
    .unwrap();
    let _stop = Stop(home.path());
    let harness = common::desktop::start(home.path());
    playwright(
        home.path(),
        "desktop-chat.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}
