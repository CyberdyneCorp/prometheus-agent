//! `prometheus version [--json]` and the `--json` output contract, run
//! against the built binary. Snapshots live in `tests/snapshots/`; values
//! that differ per build (version, ids, dates, target) are replaced by
//! placeholders after their shape is checked.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_prometheus");

fn run(args: &[&str]) -> Output {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("output-contract-home");
    Command::new(BIN)
        .args(args)
        .env("PROMETHEUS_HOME", home)
        .output()
        .expect("binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

fn snapshot(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    std::fs::read_to_string(path).expect("snapshot exists")
}

/// The single JSON document `version --json` prints.
fn version_json() -> serde_json::Map<String, Value> {
    let out = run(&["version", "--json"]);
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "diagnostics only on stderr, none expected"
    );
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "one document: {text:?}");
    match serde_json::from_str(&text).expect("valid JSON") {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn str_field<'a>(doc: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    doc[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is a string"))
}

/// Replaces the per-build values in `text` by placeholders.
fn normalize(text: &str, doc: &serde_json::Map<String, Value>) -> String {
    let mut text = text.to_owned();
    for (key, placeholder) in [
        ("build_id", "<BUILD_ID>"),
        ("build_date", "<BUILD_DATE>"),
        ("commit", "<COMMIT>"),
        ("target", "<TARGET>"),
        ("version", "<VERSION>"),
    ] {
        text = text.replace(str_field(doc, key), placeholder);
    }
    let tier = doc["tier"].to_string();
    text = text.replace(&format!("\"tier\":{tier}"), "\"tier\":<TIER>");
    let tier_text = if doc["tier"].is_null() {
        "unsupported".to_owned()
    } else {
        tier
    };
    text = text.replace(&format!("tier: {tier_text}"), "tier: <TIER>");
    for profile in ["debug", "release"] {
        text = text.replace(&format!("profile: {profile}"), "profile: <PROFILE>");
    }
    text
}

#[test]
fn version_json_has_exactly_the_documented_keys() {
    let doc = version_json();
    let raw = stdout(&run(&["version", "--json"]));
    assert_eq!(normalize(&raw, &doc), snapshot("version.json"));
}

#[test]
fn version_json_values_have_their_shapes() {
    let doc = version_json();
    let (major, minor) = str_field(&doc, "rpc_protocol")
        .split_once('.')
        .expect("<major>.<minor>");
    assert!(major.parse::<u16>().is_ok() && minor.parse::<u16>().is_ok());
    let build_id = str_field(&doc, "build_id");
    assert_eq!(build_id.len(), 32);
    assert!(build_id.chars().all(|c| c.is_ascii_hexdigit()));
    let date = str_field(&doc, "build_date");
    assert!(date.parse::<jiff::Timestamp>().is_ok(), "RFC 3339: {date}");
    assert_eq!(str_field(&doc, "version"), env!("CARGO_PKG_VERSION"));
    let tier = &doc["tier"];
    assert!(tier.is_null() || tier == 1 || tier == 2, "tier {tier}");
}

#[test]
fn verbose_text_lists_build_details_and_surfaces() {
    let doc = version_json();
    let out = run(&["version", "--verbose"]);
    assert!(out.status.success());
    assert_eq!(
        normalize(&stdout(&out), &doc),
        snapshot("version-verbose.txt")
    );
}

#[test]
fn plain_version_is_the_identity_line() {
    let out = run(&["version"]);
    let identity = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), stdout(&identity));
}

#[test]
fn json_flag_is_global() {
    assert_eq!(
        stdout(&run(&["--json", "version"])),
        stdout(&run(&["version", "--json"]))
    );
}

#[test]
fn json_usage_error_goes_to_stderr_as_one_document() {
    let out = run(&["--json", "no-such-command"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "nothing on stdout for a failure");
    let err: Value = serde_json::from_slice(&out.stderr).expect("JSON error document");
    assert_eq!(err["error"]["kind"], "usage");
    assert_eq!(err["error"]["code"], "cli.usage");
    assert_eq!(
        err["error"]["message"],
        "unrecognized subcommand 'no-such-command'"
    );
}

#[test]
fn text_usage_error_exits_2() {
    let out = run(&["no-such-command"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}
