//! The identity line is the contract shared by `--version`, `/version` and the
//! `pmt` alias, so it is tested against the built binary rather than in-process.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_prometheus");

fn version_line(exe: &Path, flag: &str) -> String {
    let out = Command::new(exe).arg(flag).output().expect("binary runs");
    assert!(out.status.success(), "{flag} exited with {:?}", out.status);
    String::from_utf8(out.stdout).expect("utf-8 output")
}

/// A hard-linked `pmt` next to the built binary, the same shape
/// `scripts/dev-install` creates; one directory per test, since tests run
/// concurrently.
fn pmt_alias(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("alias")
        .join(test);
    std::fs::create_dir_all(&dir).expect("alias dir");
    let alias = dir.join(format!("pmt{}", std::env::consts::EXE_SUFFIX));
    let _ = std::fs::remove_file(&alias);
    std::fs::hard_link(BIN, &alias)
        .or_else(|_| std::fs::copy(BIN, &alias).map(drop))
        .expect("alias created");
    alias
}

/// One line naming version, channel, target, commit and build date
/// (spec: cli, Binary names; distribution-and-update, Release identity grammar).
#[test]
fn version_line_names_version_channel_target_commit_and_build_date() {
    let line = version_line(Path::new(BIN), "--version");
    let out = Command::new(BIN)
        .args(["version", "--json"])
        .output()
        .expect("binary runs");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("version --json");
    let field = |key: &str| json[key].as_str().expect(key).to_owned();
    let expected = format!(
        "prometheus {} ({}, {}, {}, {})\n",
        field("version"),
        field("channel"),
        field("target"),
        field("commit"),
        field("build_date"),
    );
    assert_eq!(line, expected);
    assert_eq!(field("version"), env!("CARGO_PKG_VERSION"));
}

#[test]
fn short_flag_matches_long_flag() {
    let exe = Path::new(BIN);
    assert_eq!(version_line(exe, "-V"), version_line(exe, "--version"));
}

#[test]
fn pmt_alias_prints_the_same_identity_line() {
    let alias = pmt_alias("identity");
    assert_eq!(
        version_line(&alias, "--version"),
        version_line(Path::new(BIN), "--version")
    );
}

/// Scenario: Alias invocation (`pmt version --json` prints the same object).
#[test]
fn pmt_alias_prints_the_same_version_json() {
    let alias = pmt_alias("json");
    let json = |exe: &Path| {
        let out = Command::new(exe)
            .args(["version", "--json"])
            .output()
            .expect("binary runs");
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("version --json")
    };
    assert_eq!(json(&alias), json(Path::new(BIN)));
}
