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

/// Scenario: RTL layout (i18n, Language switchers show endonyms): the
/// settings list every language by endonym; choosing `ar` writes
/// `display.language` and mirrors the window at once.
#[test]
fn the_language_switcher_mirrors_the_window_for_arabic() {
    let home = mock_home();
    drive(home.path(), "desktop-language.spec.ts");
    let language = ok(&run(home.path(), &["config", "get", "display.language"]));
    assert_eq!(language.trim().trim_matches('"'), "en", "switched back");
}

/// The Bots pane (spec: bot-mode). Scenario: Title follows the bot to a
/// second desktop. Scenario: Deterministic default face (two windows draw
/// the identical face). Roster hiding and sections, creating, duplicating
/// and deleting bots, and routines whose schedule picker writes strings
/// `prometheus cron` accepts: the routines the pane added are ordinary
/// jobs of the bot's profile, delivered into its Bot Chat.
#[test]
fn the_bots_pane_manages_bots_and_their_routines() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    for bot in ["scribe", "research"] {
        ok(&run(home.path(), &["profile", "create", bot]));
    }
    drive(home.path(), "desktop-bots.spec.ts");
    let jobs = ok(&run(
        home.path(),
        &["-p", "scribe", "--json", "cron", "list"],
    ));
    let jobs: serde_json::Value = serde_json::from_str(&jobs).expect("cron list --json");
    let listed = jobs["jobs"].as_array().expect("jobs");
    let named = |name: &str| {
        listed
            .iter()
            .find(|j| j["name"] == format!("[bot:scribe] {name}"))
            .unwrap_or_else(|| panic!("{name} in {jobs}"))
    };
    let digest = named("morning digest").to_string();
    assert!(
        digest.contains("0 8 * * *") && digest.contains("bot_chat"),
        "{digest}"
    );
    assert_eq!(named("standup")["state"], "paused", "{jobs}");
    assert!(
        listed.iter().all(|j| j["name"] != "[bot:scribe] pulse"),
        "removed routine still listed: {jobs}"
    );
    let profiles = ok(&run(home.path(), &["profile", "list"]));
    assert!(
        profiles.contains("scout") && !profiles.contains("scout-2"),
        "{profiles}"
    );
}
