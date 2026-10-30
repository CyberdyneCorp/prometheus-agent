//! Borrowed CLI logins and credential benches as `prometheus auth` and
//! `prometheus model explain` show them (spec: model-providers, Borrowed
//! CLI logins; Credential management commands; Exhausted credential
//! cooldowns).

mod common;

use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use serde_json::{Value, json};

use common::{ok, run, run_with_stdin, stderr, stdout, temp_home, user_home};

const CLAUDE_TOKEN: &str = "sk-ant-oat01-borrowed-claude-code-token";

/// A Claude Code login in the test user's home.
fn claude_login(home: &Path) -> PathBuf {
    let dir = user_home(home).join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(".credentials.json");
    let login = json!({"claudeAiOauth": {
        "accessToken": CLAUDE_TOKEN, "refreshToken": "sk-ant-ort01-borrowed-refresh",
        "expiresAt": 4_102_444_800_000_i64, "subscriptionType": "max"
    }});
    std::fs::write(&file, login.to_string()).unwrap();
    file
}

fn listed(home: &Path, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--json", "auth", "list"];
    all.extend_from_slice(args);
    let out: Value = serde_json::from_str(&ok(&run(home, &all))).unwrap();
    out["credentials"].as_array().unwrap().clone()
}

#[test]
fn a_claude_code_login_is_listed_and_used_as_borrowed() {
    let home = temp_home();
    let file = claude_login(home.path());
    let rows = listed(home.path(), &[]);
    let row = rows
        .iter()
        .find(|r| r["label"] == "borrowed:claude-code")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["provider"], "anthropic");
    assert_eq!(row["kind"], "subscription");
    assert_eq!(row["source"], "borrowed");
    assert_eq!(row["status"], "ok");
    let out = run(
        home.path(),
        &[
            "model",
            "explain",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-5",
        ],
    );
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("borrowed:claude-code"), "{text}");
    assert!(!text.contains("borrowed-claude-code-token"), "{text}");
    // Prometheus never deletes another CLI's login.
    let out = run(
        home.path(),
        &["auth", "remove", "anthropic", "borrowed:claude-code"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(file.exists());
}

#[test]
fn borrowing_disabled_ignores_the_login() {
    let home = temp_home();
    claude_login(home.path());
    let off = ["--set", "auth.adopt_external_logins=false"];
    let rows = listed(home.path(), &off);
    assert!(rows.iter().all(|r| r["source"] != "borrowed"), "{rows:?}");
    let out = run(
        home.path(),
        &[
            "--set",
            "auth.adopt_external_logins=false",
            "model",
            "explain",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-5",
        ],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("no credential"), "{}", stderr(&out));
}

#[test]
fn a_benched_key_shows_its_cooldown() {
    let home = temp_home();
    let out = run_with_stdin(
        home.path(),
        &["auth", "add", "openrouter"],
        "sk-or-v1-0123456789abcdef",
    );
    ok(&out);
    let root = Home::open(home.path()).unwrap();
    let profile = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    let store = pmt_store::Store::open(&scope).unwrap();
    let now = Timestamp::now().as_millisecond();
    let until = Timestamp::from_millisecond(now).unwrap() + SignedDuration::from_mins(30);
    store
        .bench_credential(
            "openrouter",
            "secret:OPENROUTER_API_KEY",
            Some(until),
            Some("rate_limit"),
        )
        .unwrap();
    drop(store);
    let rows = listed(home.path(), &[]);
    let row = rows.iter().find(|r| r["provider"] == "openrouter").unwrap();
    assert_eq!(row["status"], "benched");
    assert_eq!(row["cooldown_until"], until.to_string());
    let out = run(
        home.path(),
        &[
            "model",
            "explain",
            "--provider",
            "openrouter",
            "--model",
            "x/y",
        ],
    );
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(stderr(&out).contains("benched"), "{}", stderr(&out));
}
