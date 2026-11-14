//! The desktop app's page end to end (spec: desktop-app): each test sets
//! up a home and the desktop browser harness (`common/desktop.rs`: the
//! page in Chromium at the webview origin, its `shell` command served by
//! the real shell core with the real binary as the bundled one), then
//! runs one Playwright spec of `web/e2e`. The desktop starts the daemon
//! itself, through `daemon ensure`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use common::dashboard::playwright;
use common::{mock_home, ok, run};

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

fn drive(home: &std::path::Path, spec: &str) {
    let _stop = Stop(home);
    let harness = common::desktop::start(home);
    playwright(home, spec, &[("PMT_DESKTOP_HARNESS", harness.url.as_str())]);
}

/// Scenario: New chat. Scenario: Replayed ticket (the ticket is never in
/// the page's storage). Scenario: Artifact script opens a window.
/// Scenario: javascript link from the agent. Scenario: Link with a prompt
/// payload.
#[test]
fn the_desktop_attaches_through_the_bundled_binary() {
    let home = mock_home();
    drive(home.path(), "desktop-attach.spec.ts");
}

/// Scenario: Reconnect re-advertises. Scenario: Resume during reconnect.
#[test]
fn the_desktop_reconnects_and_keeps_its_session() {
    let home = mock_home();
    drive(home.path(), "desktop-reconnect.spec.ts");
}

/// Scenario: Subcommand available, parent not. Scenario: Terminal-only
/// command. Scenario: Two windows open the same bot during a restart.
/// Scenario: Clearing the subagent route. Scenario: Restart gateway under
/// a profile.
#[test]
fn desktop_settings_palette_bots_and_profiles() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "coder"]));
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    for (key, value) in [
        ("delegation.model", "mock/mock"),
        ("delegation.base_url", "http://127.0.0.1:9/v1"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    drive(home.path(), "desktop-settings.spec.ts");
}
