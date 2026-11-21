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
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_store::{LineageKind, NewSession, Source, Store, Tokens};

use common::dashboard::playwright;
use common::{append_config, daemon, mock_home, ok, run};

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

/// Scenario: Status page exists (the pages beside it): `/plugins`, `/mcp`,
/// `/cron` and `/memory` show the profile's plugins, MCP servers, cron
/// jobs and memory entries from the daemon.
#[test]
fn the_plugin_mcp_cron_and_memory_pages_show_the_profiles_data() {
    let home = mock_home();
    let plugin = home.path().join("profiles/default/plugins/weather");
    pmt_plugins::testkit::install("example-weather", &plugin);
    std::fs::copy(
        Path::new(common::dashboard::WEB).join("../plugins/examples/weather/plugin.toml"),
        plugin.join("plugin.toml"),
    )
    .unwrap();
    append_config(
        home.path(),
        "\n[mcp.servers.docs]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"true\"]\n",
    );
    let added = ok(&run(
        home.path(),
        &[
            "--json",
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "nightly report",
            "--yes",
        ],
    ));
    let added: serde_json::Value = serde_json::from_str(&added).unwrap();
    let job = added["job"]["name"].as_str().unwrap().to_owned();
    ok(&run(
        home.path(),
        &["memory", "add", "The deploy window is Tuesday."],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "catalogs.spec.ts", &[("PMT_CRON_JOB", &job)]);
}

/// A home with evolution on (session lessons too), the evolution mock
/// (plus a chat answer to "hello") and one finished k = 1 job on a ledger
/// task in domain `accounting`; the pending proposal's id.
#[cfg(feature = "evolve")]
fn evolved_home() -> (common::TestHome, String) {
    let home = mock_home();
    for (key, value) in [
        ("evolve.enabled", "true"),
        ("experience.session_lessons", "true"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let mut script = include_str!("fixtures/evolve/mock.toml").to_owned();
    script.push_str("\n[[rule]]\nuser = \"hello\"\ntext = \"hi there\"\nrepeat = true\n");
    std::fs::write(home.path().join("profiles/default/mock.toml"), script).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let ws = home.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("ledger.csv"), "amount\n10.00\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let out = std::process::Command::new("git")
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .arg("-C")
            .arg(&ws)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }
    let task = home.path().join("ledger-task.toml");
    std::fs::write(
        &task,
        "statement = \"TASK-SUM the ledger\"\ndomain = \"accounting\"\nworkspace = \"ws\"\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &[
            "evolve",
            "run",
            "--task",
            task.to_str().unwrap(),
            "--k",
            "1",
        ],
    ));
    let proposals: serde_json::Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "evolve", "proposals"]))).unwrap();
    let id = proposals[0]["id"].as_str().unwrap().to_owned();
    (home, id)
}

/// The text of session `id`'s frozen system prompt.
#[cfg(feature = "evolve")]
fn prompt_text(home: &Path, id: &str) -> String {
    let session = pmt_core::id::SessionId::parse(id.trim()).unwrap();
    store(home)
        .session_prompt(session)
        .unwrap()
        .map(|p| {
            p.tiers
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Scenario: Adopting a harness.
#[cfg(feature = "evolve")]
#[test]
fn a_harness_adopted_from_the_dashboard_applies_to_new_sessions_only() {
    let (home, _) = evolved_home();
    let _daemon = daemon::start(home.path(), &[]);
    let live_file = home.path().join("live-session");
    playwright(
        home.path(),
        "evolution.spec.ts",
        &[("PMT_LIVE_SESSION_FILE", live_file.to_str().unwrap())],
    );
    let live = std::fs::read_to_string(&live_file).unwrap();
    let adopted = "Always state the total with two decimals.";
    let before = prompt_text(home.path(), &live);
    assert!(!before.is_empty(), "the live session froze a prompt");
    assert!(
        !before.contains(adopted),
        "the live session's prompt is unchanged"
    );
    ok(&run(home.path(), &["-z", "hello"]));
    let next = common::newest_session(home.path());
    assert!(
        prompt_text(home.path(), &next).contains(adopted),
        "a new session runs the adopted harness"
    );
}

/// Scenario: Removing a bad lesson.
#[cfg(feature = "evolve")]
#[test]
fn a_lesson_deleted_from_the_dashboard_is_not_offered_to_later_sessions() {
    let (home, proposal) = evolved_home();
    ok(&run(home.path(), &["evolve", "adopt", &proposal]));
    let layer = "## Lessons from earlier work in accounting";
    ok(&run(
        home.path(),
        &["-z", "hello", "--domain", "accounting"],
    ));
    let before = common::newest_session(home.path());
    assert!(
        prompt_text(home.path(), &before).contains(layer),
        "the lesson was offered"
    );
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "experience.spec.ts", &[]);
    ok(&run(
        home.path(),
        &["-z", "hello", "--domain", "accounting"],
    ));
    let after = common::newest_session(home.path());
    assert_ne!(before, after);
    assert!(
        !prompt_text(home.path(), &after).contains(layer),
        "a session started after the deletion gets no lesson"
    );
}
