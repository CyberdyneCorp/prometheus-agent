//! Natural-language schedules through the CLI and the `cronjob` tool,
//! translated by a mock auxiliary `schedule` route (spec: scheduled-jobs,
//! Natural-language schedule translation).

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{mock_home, ok, run, stderr, stdout};
use pmt_core::id::SessionId;
use serde_json::{Value, json};

const BIWEEKLY: &str = "RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0";

/// A home whose auxiliary `schedule` model answers `reply` to everything.
fn home_translating(reply: &str) -> tempfile::TempDir {
    let home = mock_home();
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("schedule.toml"),
        format!("[[rule]]\nrepeat = true\ntext = {reply:?}\n"),
    )
    .unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"schedule.toml\"\n\n[auxiliary.schedule]\nprovider = \"mock\"\n\
         model = \"schedule-translator\"\n\n[cron]\ntimezone = \"UTC\"\n",
    );
    home
}

fn jobs(home: &Path) -> Vec<Value> {
    let doc: Value = serde_json::from_str(&ok(&run(home, &["--json", "cron", "list"]))).unwrap();
    doc["jobs"].as_array().cloned().unwrap_or_default()
}

/// Scenario: Biweekly phrase becomes an RRULE.
#[test]
fn a_biweekly_phrase_is_echoed_as_an_rrule_and_saved_only_once_confirmed() {
    let home = home_translating(BIWEEKLY);
    let nl = [
        "cron",
        "add",
        "--nl",
        "every other thursday at noon",
        "--prompt",
        "standup notes",
    ];
    let out = run(home.path(), &nl);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let echo = stdout(&out);
    assert!(
        echo.starts_with(
            "translated \"every other thursday at noon\" to:\n\
             rrule FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0 (UTC)\n"
        ),
        "{echo}"
    );
    assert_eq!(echo.matches("  next: Thu ").count(), 3, "{echo}");
    assert!(echo.contains("12:00 UTC"), "{echo}");
    assert!(
        jobs(home.path()).is_empty(),
        "nothing saved without confirmation"
    );

    let mut confirmed = nl.to_vec();
    confirmed.push("--yes");
    ok(&run(home.path(), &confirmed));
    let saved = jobs(home.path());
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0]["schedule"],
        "rrule FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0"
    );
    // The translation call is accounted to the auxiliary task.
    let usage = ok(&run(home.path(), &["--json", "usage", "--by", "purpose"]));
    assert!(usage.contains("auxiliary.schedule"), "{usage}");
}

/// Scenario: Translation is never trusted silently.
#[test]
fn an_answer_that_is_neither_grammar_nor_a_valid_rrule_saves_nothing() {
    for reply in ["whenever the moon is full", "RRULE:FREQ=DAILY;BYHOUR=25"] {
        let home = home_translating(reply);
        let out = run(
            home.path(),
            &[
                "cron",
                "add",
                "--nl",
                "when the moon is full",
                "--prompt",
                "x",
                "--yes",
            ],
        );
        assert_ne!(out.status.code(), Some(0));
        assert!(
            stderr(&out).contains("could not translate schedule: when the moon is full"),
            "{reply}: {}",
            stderr(&out)
        );
        assert!(jobs(home.path()).is_empty());
    }
}

/// `cron edit --nl` translates and echoes the new schedule the same way.
#[test]
fn an_edit_takes_a_translated_schedule() {
    let home =
        home_translating("RRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0");
    ok(&run(
        home.path(),
        &[
            "cron", "add", "every 1d", "--prompt", "x", "--name", "report", "--yes",
        ],
    ));
    let out = run(
        home.path(),
        &[
            "cron",
            "edit",
            "report",
            "--nl",
            "first business day of each month at 8",
            "--yes",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out)
            .contains("rrule FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0")
    );
    assert_eq!(
        jobs(home.path())[0]["schedule"],
        "rrule FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1;BYHOUR=8;BYMINUTE=0"
    );
}

/// The tool's `schedule_text` needs the confirmation round trip, and the
/// confirming call saves exactly the translation that was echoed.
#[test]
fn the_tool_translates_once_and_saves_what_it_echoed() {
    let home = home_translating(BIWEEKLY);
    let scope = {
        use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
        let home = Home::open(home.path()).unwrap();
        let profile = home
            .existing_profile(&ProfileName::parse("default").unwrap())
            .unwrap();
        ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
    };
    let store = Arc::new(pmt_store::Store::open(&scope).unwrap());
    let id: SessionId = store
        .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
        .unwrap()
        .id;
    let launch = pmt_core::env::LaunchEnv::default();
    let caller = prometheus::cron_host::caller::Caller::default();
    let port = prometheus::cron_host::tool::port(&scope, &launch, &store, true, caller);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let call = |args: Value| -> Value {
        let result = runtime.block_on(port.call(id, args));
        serde_json::from_str(&result.text_content())
            .unwrap_or_else(|_| json!({"error": result.text_content()}))
    };
    let mut args =
        json!({"action": "create", "schedule_text": "every other thursday at noon", "prompt": "x"});
    let first = call(args.clone());
    assert_eq!(first["status"], "needs_confirmation", "{first}");
    assert_eq!(first["translated_from"], "every other thursday at noon");
    assert_eq!(first["echo"]["kind"], "rrule");
    // The model now answers something else: the confirmation still saves
    // the echoed translation.
    std::fs::write(
        home.path().join("profiles/default/schedule.toml"),
        "[[rule]]\nrepeat = true\ntext = \"every 5m\"\n",
    )
    .unwrap();
    args["confirm_token"] = first["confirm_token"].clone();
    let saved = call(args);
    assert_eq!(saved["status"], "created", "{saved}");
    assert_eq!(
        saved["job"]["schedule"],
        "rrule FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;BYHOUR=12;BYMINUTE=0"
    );
}
