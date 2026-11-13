//! Windows, tabs, modes, terminals and the live subagents frame end to
//! end (spec: desktop-app, Windows, tabs and panes; Interface modes and
//! layouts; Embedded terminals are daemon-owned PTYs; Live subagents
//! frame): the page in the desktop browser harness against the real
//! daemon, driving `web/e2e/desktop-workspace.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use common::dashboard::playwright;
use common::{append_config, mock_home, run};

const SCRIPT: &str = r#"
[[rule]]
user = "count slowly"
latency_ms = 3000
text = "counted to ten"

[[rule]]
user = "delegate the slow job"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "slow child job" }] }

[[rule]]
user = "slow child job"
latency_ms = 30000
text = "child done"
repeat = true

[[rule]]
repeat = true
text = "ok"
"#;

struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// Scenario: Popped-out session. Scenario: Returning to advanced mode.
/// Scenario: Steering is acknowledged as queued.
#[test]
fn desktop_windows_tabs_modes_terminals_and_subagents() {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), SCRIPT).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let _stop = Stop(home.path());
    let harness = common::desktop::start(home.path());
    playwright(
        home.path(),
        "desktop-workspace.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}
