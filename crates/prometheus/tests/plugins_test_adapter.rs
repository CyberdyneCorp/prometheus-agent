//! `prometheus plugins test-adapter <plugin dir>` (spec: platform-adapters,
//! Adapter conformance kit): a `platform-adapter` plugin runs the same
//! suite as a first-party adapter, against the kit's reference platform,
//! under exactly the grants it declares; one missing a contract operation
//! is refused naming it.

#![cfg(feature = "gateway")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use self::common::{command, stderr, stdout};

/// Runs `args` with the kit's scratch home inside the test's home.
fn run_kit(home: &Path, args: &[&str]) -> Output {
    let scratch = home.join("tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    command(home)
        .args(args)
        .env("TMPDIR", &scratch)
        .env("TMP", &scratch)
        .env("TEMP", &scratch)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// The test plugin `component` as plugin `mattermost` in `home`, declaring
/// only its real platform's host.
fn plugin(home: &Path, component: &str) -> PathBuf {
    let dir = home.join("mattermost");
    pmt_plugins::testkit::install(component, &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"mattermost\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Mattermost adapter\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"platform-adapter@1.0\"]\n\n[capabilities]\nnetwork = [\"chat.example.com:443\"]\n",
    )
    .unwrap();
    dir
}

#[test]
fn a_plugin_adapter_passes_the_suite_under_its_own_grants() {
    let home = tempfile::tempdir().unwrap();
    let dir = plugin(home.path(), "testing-adapter");
    let out = run_kit(
        home.path(),
        &["plugins", "test-adapter", dir.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", stderr(&out));
    for check in [
        "descriptor",
        "connect",
        "inbound normalization",
        "deduplication",
        "reconnect",
        "splitting",
        "retry classification",
        "chat info",
        "capability grants",
    ] {
        assert!(
            text.lines().any(|l| l == format!("pass  {check}")),
            "did not pass {check}:\n{text}"
        );
    }
    for capability in ["edit", "typing", "reactions", "cards"] {
        assert!(
            text.lines()
                .any(|l| l.starts_with(&format!("skip  {capability}: "))),
            "printed no line for {capability}:\n{text}"
        );
    }
    assert!(!text.contains("FAIL"), "{text}");
    let scratch = std::fs::read_dir(home.path().join("tmp")).unwrap().count();
    assert_eq!(scratch, 0, "the kit's scratch home is removed");
}

/// Scenario: Plugin missing a required export (through the kit).
#[test]
fn a_plugin_adapter_missing_an_operation_is_refused_naming_it() {
    let home = tempfile::tempdir().unwrap();
    let dir = plugin(home.path(), "testing-adapter-nosend");
    let out = run_kit(
        home.path(),
        &["plugins", "test-adapter", dir.to_str().unwrap()],
    );
    assert_ne!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("does not export the required operation 'send'"),
        "{}",
        stderr(&out)
    );
}
