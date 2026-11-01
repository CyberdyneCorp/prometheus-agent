//! `prometheus prompt size` and the `/personality` body (spec:
//! agent-turn-loop, Offline prompt size report; Personality overlays).

mod common;

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use prometheus::slash_handlers::personality;
use serde_json::Value;

use common::{command, ok, run, stderr, temp_home};

fn sum(values: &Value, key: &str) -> u64 {
    values
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v[key].as_u64().unwrap())
        .sum()
}

#[test]
fn the_json_breakdown_sums_to_the_request_size_without_network() {
    let home = temp_home();
    let out = ok(&command(home.path())
        .env("PMT_LOG", "debug")
        .args(["prompt", "size", "--platform", "telegram", "--json"])
        .output()
        .unwrap());
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["schema"], "prometheus.prompt_size.v1");
    assert_eq!(doc["platform"], "telegram");
    let layers = &doc["layers"];
    let names: Vec<&str> = layers
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["layer"].as_str().unwrap())
        .collect();
    for layer in [
        "identity",
        "personality",
        "tool_guidance",
        "platform_hint",
        "skills_index",
        "project_context",
        "memory",
        "plugin_sections",
        "runtime",
    ] {
        assert!(names.contains(&layer), "{layer} missing from {names:?}");
    }
    assert!(!doc["toolsets"].as_array().unwrap().is_empty());
    let system = doc["system_bytes"].as_u64().unwrap();
    let tools = doc["tools_bytes"].as_u64().unwrap();
    assert_eq!(sum(layers, "bytes"), system);
    assert_eq!(
        sum(&doc["toolsets"], "bytes") + doc["tools_framing_bytes"].as_u64().unwrap(),
        tools
    );
    assert_eq!(doc["total_bytes"].as_u64().unwrap(), system + tools);
    assert_eq!(
        doc["total_tokens"].as_u64().unwrap(),
        (system + tools).div_ceil(4)
    );
    assert!(sum(layers, "bytes") > 0);

    for log in [
        home.path().join("logs/prometheus.log"),
        home.path().join("profiles/default/logs/prometheus.log"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&log) {
            assert!(
                !text.contains("outbound HTTP client configured"),
                "a network client was configured:\n{text}"
            );
        }
    }
}

#[test]
fn the_text_report_and_its_errors() {
    let home = temp_home();
    let text = ok(&run(home.path(), &["prompt", "size", "--toolsets", "file"]));
    assert!(text.contains("identity"), "{text}");
    assert!(text.contains("\nfile "), "{text}");
    assert!(text.trim_end().ends_with("tokens"), "{text}");

    let cron = ok(&run(
        home.path(),
        &["prompt", "size", "--platform", "cron", "--json"],
    ));
    assert!(!cron.contains("\"clarify\""), "{cron}");
    let cli = ok(&run(home.path(), &["prompt", "size", "--json"]));
    assert!(cli.contains("\"clarify\""), "{cli}");

    let out = run(home.path(), &["prompt", "size", "--platform", "fax"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("unknown platform 'fax'"),
        "{}",
        stderr(&out)
    );
    let out = run(home.path(), &["prompt", "size", "--toolsets", "nope"]);
    assert_ne!(out.status.code(), Some(0));
}

fn scope(root: &Path) -> ProfileScope {
    let home = Home::open(root).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

#[test]
fn personality_selects_for_the_next_session_or_now() {
    let home = temp_home();
    let scope = scope(home.path());
    let (listing, now) = personality(&scope, "").unwrap();
    assert!(listing.starts_with("* none (active)\n"), "{listing}");
    assert!(listing.contains("  pirate"), "{listing}");
    assert_eq!(now, None);

    let (text, now) = personality(&scope, "pirate").unwrap();
    assert_eq!(text, "personality pirate applies from the next session");
    assert_eq!(now, None);
    let reopened = scope_again(&scope);
    assert_eq!(reopened.config().agent.personality, "pirate");

    let (_, now) = personality(&reopened, "neutral --now").unwrap();
    assert_eq!(now.as_deref(), Some("none"));
    assert_eq!(scope_again(&scope).config().agent.personality, "none");

    let err = personality(&scope, "pirat").unwrap_err();
    assert_eq!(err.code().code, "personality.unknown");
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.message().contains("valid: none, helpful"),
        "{}",
        err.message()
    );
    assert_eq!(
        personality(&scope, "a b").unwrap_err().code().code,
        "cli.usage"
    );
}

fn scope_again(scope: &ProfileScope) -> ProfileScope {
    ProfileScope::open(
        scope.profile().clone(),
        &Overrides::default(),
        Launch::Other,
    )
    .unwrap()
}
