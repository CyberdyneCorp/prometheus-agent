//! The M3 dashboard pages in a real browser (spec: web-dashboard): each
//! test sets up a home and its daemon, then runs one Playwright spec of
//! `web/e2e` against it (see `dashboard_e2e.rs` for the setup it needs).

#![cfg(feature = "dashboard")]

mod common;

use common::dashboard::playwright;
use common::{daemon, mock_home};

fn webapp_home() -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[dashboard]\nport = 0\n[webapp]\nenabled = true\n",
    )
    .unwrap();
    home
}

/// Scenario: Opening from the CLI. Scenario: Sign out every tab.
#[test]
fn the_webapp_opens_from_the_cli_and_signs_out_every_tab() {
    let home = webapp_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "webapp.spec.ts", &[]);
}

/// Scenario: Secret shown once. Scenario: Offline docs.
#[test]
fn the_webhooks_and_docs_pages_work_in_the_browser() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "m3pages.spec.ts", &[]);
}
