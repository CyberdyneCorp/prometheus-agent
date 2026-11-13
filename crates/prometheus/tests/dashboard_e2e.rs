//! The dashboard in a real browser (spec: web-dashboard): each test sets
//! up a home and its daemon, then runs one Playwright spec of `web/e2e`
//! against it. The specs get a fresh login link from the CLI themselves
//! (`PMT_BIN`, `PMT_HOME`). They need `npm ci` in `web/` and Playwright's
//! Chromium (`npx playwright install chromium --only-shell`), which
//! `scripts/check` and CI provide.

#![cfg(feature = "dashboard")]

mod common;

use std::fmt::Write as _;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_store::{LineageKind, NewSession, Source, Store, Tokens};

use common::{BIN, append_config, daemon, mock_home, ok, run};

const WEB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web");

/// Runs `web/e2e/<spec>` against `home`'s running daemon.
fn playwright(home: &Path, spec: &str, extra: &[(&str, &str)]) {
    let web = Path::new(WEB);
    let cli = web.join("node_modules/@playwright/test/cli.js");
    assert!(
        cli.is_file(),
        "the dashboard's browser tests need `npm ci` in web/ (scripts/check runs it)"
    );
    let out = Command::new("node")
        .current_dir(web)
        .arg(&cli)
        .args([
            "test",
            &format!("e2e/{spec}"),
            "--config",
            "playwright.config.ts",
        ])
        .env("PMT_BIN", BIN)
        .env("PMT_HOME", home)
        .env("PMT_USER_HOME", common::user_home(home))
        .env("PMT_PW_OUT", home.join("playwright"))
        .envs(extra.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{spec} failed\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The mock answers "clean ..." with a terminal call that needs approval
/// and a refused call with "refused".
fn approval_script(home: &Path, profile: &str) {
    let script = home.join(format!("{profile}-script.toml"));
    std::fs::write(
        &script,
        "[[rule]]\ntool_result_regex = \"^BLOCKED: \"\ntext = \"refused\"\nrepeat = true\n\
         [[rule]]\nuser = \"clean\"\nrepeat = true\n[[rule.tool_calls]]\nname = \"terminal\"\n\
         arguments = { command = \"rm -rf build\" }\n\
         [[rule]]\nuser = \"hello\"\ntext = \"hi there\"\nrepeat = true\n",
    )
    .unwrap();
    let config = home.join(format!("profiles/{profile}/config.toml"));
    let mut text = std::fs::read_to_string(&config).unwrap();
    let _ = write!(
        text,
        "\n[providers.mock]\nscript = {:?}\n",
        script.to_string_lossy()
    );
    std::fs::write(&config, text).unwrap();
    std::fs::create_dir_all(home.join(format!("profiles/{profile}/workspace/build"))).unwrap();
}

/// Scenario: Approval from the browser.
#[test]
fn an_approval_is_answered_from_the_browser_over_the_client_protocol() {
    let home = mock_home();
    approval_script(home.path(), "default");
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "chat.spec.ts", &[]);
}

/// Scenario: Approval badge.
#[test]
fn a_session_waiting_for_approval_shows_a_badge_on_the_sessions_page() {
    let home = mock_home();
    common::api::profile(home.path(), "work", "");
    common::api::multiplex(home.path());
    approval_script(home.path(), "work");
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "events.spec.ts", &[]);
}

/// Scenario: OIDC viewer submits.
#[test]
fn an_oidc_viewer_sees_a_disabled_composer_and_cannot_submit() {
    let home = mock_home();
    let issuer = common::oidc::start("viewer@example.com");
    std::fs::write(
        home.path().join("daemon.toml"),
        common::oidc::daemon_toml(&issuer, &["viewer@example.com"], &[]),
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "observer.spec.ts", &[]);
}

/// Scenario: Status page exists. Built-in pages.
#[test]
fn the_status_page_and_every_built_in_route_render() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "status.spec.ts", &[]);
    playwright(home.path(), "pages.spec.ts", &[]);
}

/// Scenario: Reading a key.
#[test]
fn secrets_set_in_the_browser_are_never_shown_again() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "secrets.spec.ts", &[]);
}

/// Scenario: Invalid value.
#[test]
fn the_config_page_refuses_an_invalid_value_and_keeps_the_file() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "config.spec.ts", &[]);
}

/// Logs page is redacted and bounded.
#[test]
fn the_logs_page_tails_registry_logs() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    ok(&run(home.path(), &["daemon", "status"]));
    playwright(home.path(), "logs.spec.ts", &[]);
}

fn store(home: &Path) -> Arc<Store> {
    let home = Home::open(home).unwrap();
    let dir = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

fn spend(recorder: &UsageRecorder, session: pmt_core::id::SessionId, usd: f64) {
    recorder
        .record_at(
            Call {
                tokens: Tokens {
                    input: 1000,
                    output: 100,
                    ..Tokens::default()
                },
                reported_usd: Some(usd),
                billing_source: "api_key:test".to_owned(),
                ..Call::new(session, "main", "test", "m1")
            },
            Timestamp::now(),
        )
        .unwrap();
}

/// Scenario: Delegation cost rolls up. Scenario: Budget warning.
#[test]
fn the_usage_page_rolls_children_up_and_shows_the_budget() {
    let home = mock_home();
    append_config(home.path(), "\n[budgets]\nmonthly_usd = 20.0\n");
    let store = store(home.path());
    let recorder = UsageRecorder::new(Arc::clone(&store), Arc::new(ConfigPrices::default()));
    let root = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    spend(&recorder, root, 0.10);
    for usd in [0.25, 0.15] {
        let child = NewSession::child(Source::Cli, root, LineageKind::Delegate);
        let child = store.create_session(&child).unwrap().id;
        spend(&recorder, child, usd);
    }
    let other = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    spend(&recorder, other, 18.00);
    drop(recorder);
    drop(store);
    let _daemon = daemon::start(home.path(), &[]);
    playwright(
        home.path(),
        "usage.spec.ts",
        &[("PMT_ROOT_SESSION", &root.to_string())],
    );
}
