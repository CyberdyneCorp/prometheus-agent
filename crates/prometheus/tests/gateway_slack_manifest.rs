//! `prometheus gateway slack-manifest` (spec: platform-adapters, Slack app
//! manifest generator).

#![cfg(feature = "gateway")]

mod common;

use serde_json::Value;

use self::common::{ok, run, stderr, temp_home};

fn manifest(home: &std::path::Path, args: &[&str]) -> Value {
    let mut all = vec!["gateway", "slack-manifest"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

#[test]
fn commands_only_prints_one_slash_command_per_gateway_command() {
    let home = temp_home();
    let commands = manifest(home.path(), &["--commands-only"]);
    let commands = commands.as_array().expect("a JSON array and nothing else");
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    for expected in ["/prometheus", "/new", "/stop", "/approve", "/whoami"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    let unique: std::collections::BTreeSet<&&str> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "one entry per command");
    assert!(commands.iter().all(|c| c["description"].is_string()));
}

#[test]
fn the_manifest_enables_socket_mode_with_the_bot_scopes() {
    let home = temp_home();
    let full = manifest(home.path(), &["--name", "Ops Bot"]);
    assert_eq!(full["display_information"]["name"], "Ops Bot");
    assert_eq!(full["settings"]["socket_mode_enabled"], true);
    let scopes = full["oauth_config"]["scopes"]["bot"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "chat:write"));
    assert!(scopes.iter().any(|s| s == "assistant:write"));
    assert!(full["features"]["slash_commands"].as_array().unwrap().len() > 3);
    let lean = manifest(home.path(), &["--no-assistant", "--agent-view"]);
    let scopes = lean["oauth_config"]["scopes"]["bot"].as_array().unwrap();
    assert!(!scopes.iter().any(|s| s == "assistant:write"));
    assert!(lean["features"]["assistant_view"].is_null());
    let events = lean["settings"]["event_subscriptions"]["bot_events"]
        .as_array()
        .unwrap();
    assert!(events.iter().any(|e| e == "app_home_opened"));
}

#[test]
fn a_short_long_description_is_a_usage_error() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["gateway", "slack-manifest", "--long-description", "short"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("175"), "{}", stderr(&out));
    let long = "x".repeat(200);
    let full = manifest(home.path(), &["--long-description", &long]);
    assert_eq!(full["display_information"]["long_description"], long);
}

#[test]
fn write_saves_the_manifest_and_an_unwritable_path_exits_73() {
    let home = temp_home();
    let out = run(home.path(), &["gateway", "slack-manifest", "--write"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let saved = home.path().join("profiles/default/slack-manifest.json");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(saved).unwrap()).unwrap();
    assert_eq!(written["settings"]["socket_mode_enabled"], true);
    let missing = home.path().join("no/such/dir/manifest.json");
    let out = run(
        home.path(),
        &[
            "gateway",
            "slack-manifest",
            "--write",
            missing.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(73), "{}", stderr(&out));
}
