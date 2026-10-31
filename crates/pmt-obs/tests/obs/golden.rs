//! Golden rendering (spec: observability, Canonical text rendering): a
//! recorded trajectory renders byte-for-byte as the checked-in text.
//! `UPDATE_GOLDEN=1` rewrites the expected files after a deliberate change.

use std::path::{Path, PathBuf};

use pmt_obs::trajectory::read;
use pmt_obs::trajectory::render::{RenderOptions, render};

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/obs/golden")
        .join(name)
}

fn check(name: &str, actual: &str) {
    let path = golden(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden {name}; run with UPDATE_GOLDEN=1"));
    assert_eq!(actual, expected, "golden {name}");
}

#[test]
fn a_recorded_trajectory_renders_byte_for_byte() {
    let lines = read::read_file(&golden("recorded.jsonl")).unwrap();
    let text = render(&lines, &RenderOptions::default()).unwrap();
    check("recorded.txt", &text);
    // Deterministic: rendering again gives the same bytes.
    assert_eq!(render(&lines, &RenderOptions::default()).unwrap(), text);
}

#[test]
fn a_compressed_trajectory_renders_like_the_plain_one() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("ses_x.jsonl");
    std::fs::copy(golden("recorded.jsonl"), &plain).unwrap();
    let compressed = pmt_obs::trajectory::files::compress(&plain).unwrap();
    let lines = read::read_file(&compressed).unwrap();
    let text = render(&lines, &RenderOptions::default()).unwrap();
    assert_eq!(
        text,
        std::fs::read_to_string(golden("recorded.txt")).unwrap()
    );
}

#[test]
fn an_older_reader_renders_unknown_events_of_a_newer_minor_as_markers() {
    // recorded.jsonl contains `workspace.indexed`, written by grammar 1.3.
    let text = std::fs::read_to_string(golden("recorded.txt")).unwrap();
    assert!(text.contains("\n[EVENT workspace.indexed]\n"), "{text}");
    assert!(
        text.contains("[SUBAGENT sub_1] completed:"),
        "the rest renders normally"
    );
}

#[test]
fn a_failed_read_file_is_an_observation_error_with_kind_not_found() {
    let lines = read::read_file(&golden("recorded.jsonl")).unwrap();
    let failed = lines
        .iter()
        .find(|l| l.kind == "tool.result" && l.payload["ok"] == false)
        .unwrap();
    assert_eq!(failed.payload["error"]["kind"], "not_found");
    let text = std::fs::read_to_string(golden("recorded.txt")).unwrap();
    assert!(text.contains("[OBSERVATION ERROR]\nnot_found: no such file: notes.txt"));
}
