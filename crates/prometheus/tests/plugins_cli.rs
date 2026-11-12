//! `prometheus plugins test-adapter` (spec: platform-adapters, Adapter
//! conformance kit): the suite passes for every first-party adapter
//! against its built-in mock, printing a line per check.

#![cfg(feature = "adapter-kits")]

mod common;

use std::path::Path;
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

#[test]
fn the_kit_passes_for_all_eight_first_party_adapters() {
    let home = tempfile::tempdir().unwrap();
    for platform in [
        "telegram", "discord", "slack", "email", "matrix", "signal", "whatsapp", "webhook",
    ] {
        let out = run_kit(home.path(), &["plugins", "test-adapter", platform]);
        let text = stdout(&out);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{platform}:\n{text}\n{}",
            stderr(&out)
        );
        for check in [
            "descriptor",
            "connect",
            "inbound normalization",
            "deduplication",
            "reconnect",
            "splitting",
            "chat info",
        ] {
            assert!(
                text.contains(&format!("pass  {check}")),
                "{platform} did not pass {check}:\n{text}"
            );
        }
        assert!(!text.contains("FAIL"), "{platform}:\n{text}");
        // Every optional capability gets a line: exercised when declared,
        // skipped with a reason otherwise.
        for capability in [
            "edit",
            "typing",
            "reactions",
            "threads",
            "delete",
            "media in",
            "media out",
            "streaming tts",
            "receipts",
            "cards",
        ] {
            assert!(
                text.lines().any(|l| l == format!("pass  {capability}")
                    || l.starts_with(&format!("skip  {capability}: "))),
                "{platform} printed no line for {capability}:\n{text}"
            );
        }
    }
    let scratch = std::fs::read_dir(home.path().join("tmp")).unwrap().count();
    assert_eq!(scratch, 0, "the kit's scratch homes are removed");
}

#[test]
fn unknown_names_and_invalid_plugin_directories_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let out = run_kit(home.path(), &["plugins", "test-adapter", "myspace"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("neither a first-party adapter"),
        "{}",
        stderr(&out)
    );
    let plugin = home.path().join("my-adapter");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.toml"), "name = \"my-adapter\"\n").unwrap();
    let out = run_kit(
        home.path(),
        &["plugins", "test-adapter", plugin.to_str().unwrap()],
    );
    // A plugin directory is tested as a plugin: an invalid manifest is
    // refused as such (exit 65).
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown manifest field 'name'"),
        "{}",
        stderr(&out)
    );
}
