//! Suggested automations and blueprints (spec: scheduled-jobs, Suggested
//! automations inbox; Automation blueprint catalog; skills, Blueprint
//! skills never auto-schedule): the `cron suggestions` CLI, `/blueprint`
//! and the background review's suggestions inside the real daemon with
//! the debug-build file adapter.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
use self::common::{mock_home, ok, run, stderr};

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn jobs(home: &Path) -> Vec<Value> {
    json(home, &["cron", "list", "--all"])["jobs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn pending(home: &Path) -> Vec<Value> {
    json(home, &["cron", "suggestions"])["suggestions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn keys(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["key"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn store(home: &Path) -> pmt_store::Store {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

const DIGEST_SKILL: &str = "---\nname: daily-digest\ndescription: A digest of the day.\n\
blueprint:\n  title: Daily digest\n  schedule: \"{time.minute} {time.hour} * * *\"\n  \
prompt: Write my daily digest\n  slots:\n    - name: time\n      kind: time\n      \
default: \"18:00\"\n---\n\nSummarise the day.\n";

/// Scenario (skills): Install does not create a job; and the inbox CLI:
/// the starters, accept through the create path, dismiss, clear.
#[test]
fn the_inbox_offers_starters_and_skill_blueprints_without_creating_jobs() {
    let home = mock_home();
    let home = home.path();
    let skill = home.join("profiles/default/skills/daily-digest");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), DIGEST_SKILL).unwrap();
    let listed = pending(home);
    assert_eq!(keys(&listed), ["skill:daily-digest"], "{listed:?}");
    assert_eq!(listed[0]["source"], "blueprint");
    assert_eq!(listed[0]["schedule"], "0 18 * * *");
    assert!(jobs(home).is_empty(), "a skill never schedules by itself");

    ok(&run(home, &["cron", "suggestions", "catalog"]));
    let listed = pending(home);
    assert_eq!(listed.len(), 5, "{listed:?}");
    let refused = run(home, &["cron", "suggestions", "accept", "2"]);
    assert_eq!(refused.status.code(), Some(4), "{}", stderr(&refused));
    assert!(jobs(home).is_empty(), "nothing without confirmation");
    let accepted = json(home, &["cron", "suggestions", "accept", "2", "--yes"]);
    assert_eq!(accepted["job"]["name"], listed[1]["title"]);
    assert_eq!(jobs(home).len(), 1);
    ok(&run(home, &["cron", "suggestions", "dismiss", "1"]));
    let left = keys(&pending(home));
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(!left.contains(&listed[0]["key"].as_str().unwrap().to_owned()));
    assert!(!left.contains(&listed[1]["key"].as_str().unwrap().to_owned()));
    ok(&run(home, &["cron", "suggestions", "catalog"]));
    assert_eq!(
        pending(home).len(),
        3,
        "accepted and dismissed keys stay out"
    );
    ok(&run(home, &["cron", "suggestions", "clear"]));
}

/// Scenarios: Inline values skip the questions; Invalid slot value; and a
/// missing slot asked one question at a time.
#[test]
fn a_blueprint_echoes_its_schedule_and_saves_only_on_yes() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"ok\"\n",
        "\n[cron]\ntimezone = \"UTC\"\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(
        home,
        "default",
        "chat-1",
        "/blueprint morning-brief time=08:00 days=weekdays",
    );
    wait_said(home, "default", "cron 0 8 * * 1-5");
    assert!(jobs(home).is_empty(), "the echo saves nothing");
    send(home, "default", "chat-1", "yes");
    wait("the job is created", Duration::from_mins(1), || {
        !jobs(home).is_empty()
    });
    let job = &jobs(home)[0];
    assert_eq!(job["name"], "Morning brief");
    assert_eq!(job["schedule"], "cron 0 8 * * 1-5");
    assert_eq!(job["deliver"], json!(["origin"]));

    send(home, "default", "chat-1", "/bp morning-brief time=25:00");
    wait_said(home, "default", "invalid value for slot 'time'");
    assert_eq!(jobs(home).len(), 1, "an invalid value creates nothing");

    send(home, "default", "chat-1", "/bp custom-reminder");
    wait_said(home, "default", "What should I remind you of?");
    send(home, "default", "chat-1", "water the plants");
    wait_said(home, "default", "Reminder: Remind me: water the plants");
    send(home, "default", "chat-1", "yes");
    wait("the second job", Duration::from_mins(1), || {
        jobs(home).len() == 2
    });
    assert!(
        !texts(home, "default").iter().any(|t| t == "ok"),
        "no answer became a model turn"
    );
    daemon.stop(home);
}

const REVIEW_SCRIPT: &str = r#"
[[rule]]
user = "durable knowledge"
times = 1
[[rule.tool_calls]]
name = "suggest_automation"
arguments = { key = "weekly-standup", title = "Weekly standup", schedule = "0 9 * * 1", prompt = "Collect my standup notes" }
[[rule.tool_calls]]
name = "suggest_automation"
arguments = { key = "daily-digest", title = "Daily digest", schedule = "every 1d", prompt = "Write my daily digest" }

[[rule]]
user = "durable knowledge"
text = "nothing to save"

[[rule]]
user = "reusable know-how"
repeat = true
text = "nothing to save"

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Dismissed suggestion is not re-offered (the review proposes
/// it again; only the new one reaches the inbox).
#[test]
fn the_review_cannot_offer_a_dismissed_suggestion_again() {
    let home = gateway_home_with(REVIEW_SCRIPT, "\n[cron]\ntimezone = \"UTC\"\n");
    let home = home.path();
    let earlier = pmt_store::cron::suggestions::SuggestionRow {
        key: "weekly-standup".to_owned(),
        source: "usage".to_owned(),
        title: "Weekly standup".to_owned(),
        schedule: "0 9 * * 1".to_owned(),
        prompt: "Collect my standup notes".to_owned(),
        ..Default::default()
    };
    let store = store(home);
    assert!(store.cron_offer_suggestion(&earlier, 5).unwrap());
    ok(&run(home, &["cron", "suggestions", "dismiss", "1"]));
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "ok");
    send(home, "default", "u1", "/refine");
    // The review proposes both, in this order; only the new one lands.
    wait("the review's suggestion", Duration::from_mins(1), || {
        keys(&pending(home)).contains(&"daily-digest".to_owned())
    });
    assert_eq!(keys(&pending(home)), ["daily-digest"]);
    daemon.stop(home);
}

/// Automation blueprint catalog: an enabled plugin's
/// `[[automation_blueprints]]` join the catalog under `<plugin>:<key>`,
/// never replacing a built-in; a disabled plugin's do not.
#[test]
fn an_enabled_plugins_blueprints_join_the_catalog_namespaced() {
    let home = mock_home();
    let home = home.path();
    let dir = home.join("profiles/default/plugins/acme");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"acme\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"hook@1.0\"]\n\n\
         [[automation_blueprints]]\nkey = \"standup\"\ntitle = \"Standup notes\"\n\
         schedule = \"{time.minute} {time.hour} * * 1-5\"\nprompt = \"Collect standup notes for {team}\"\n\
         slots = [{ name = \"time\", kind = \"time\", default = \"09:15\" }, { name = \"team\", kind = \"text\" }]\n\n\
         [[automation_blueprints]]\nkey = \"morning-brief\"\ntitle = \"Not the built-in\"\n\
         schedule = \"every 1d\"\nprompt = \"x\"\n",
    )
    .unwrap();
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-03T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
    let scope = || {
        let layout = pmt_config::Home::open(home).unwrap();
        let dir = layout
            .existing_profile(&pmt_config::ProfileName::parse("default").unwrap())
            .unwrap();
        pmt_config::ProfileScope::open(
            dir,
            &pmt_config::Overrides::default(),
            pmt_config::Launch::Other,
        )
        .unwrap()
    };
    let launch = pmt_core::env::LaunchEnv::default();
    let keys_of = |scope: &pmt_config::ProfileScope| -> Vec<String> {
        prometheus::cron_host::blueprints::catalog(scope, &launch)
            .into_iter()
            .map(|b| b.key)
            .collect()
    };
    assert!(
        !keys_of(&scope()).contains(&"acme:standup".to_owned()),
        "disabled"
    );
    ok(&run(home, &["plugins", "enable", "acme", "--grant", "all"]));
    let keys = keys_of(&scope());
    assert!(keys.contains(&"acme:standup".to_owned()), "{keys:?}");
    assert!(keys.contains(&"acme:morning-brief".to_owned()), "{keys:?}");
    let catalog = prometheus::cron_host::blueprints::catalog(&scope(), &launch);
    let builtin = pmt_cron::blueprint::resolve(&catalog, "morning-brief").unwrap();
    assert_eq!(builtin.title, "Morning brief", "the built-in wins its key");
}
