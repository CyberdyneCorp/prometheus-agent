//! `cargo xtask health` against fixture trees under `tests/fixtures/health`,
//! each a minimal repository with its own `health.toml`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/health")
        .join(case)
}

/// Runs the gate on a fixture; returns whether it passed and its stderr.
fn health(case: &str, extra: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("health")
        .arg("--root")
        .arg(fixture(case))
        .args(extra)
        .output()
        .expect("xtask runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn code_within_budget_passes() {
    let (passed, stderr) = health("within_budget", &[]);
    assert!(passed, "{stderr}");
}

#[test]
fn function_over_budget_fails_naming_it_and_the_limit() {
    let (passed, stderr) = health("over_budget", &[]);
    assert!(!passed);
    assert!(
        stderr.contains(
            "crates/demo/src/lib.rs::tangled: cognitive complexity is 17, over the limit of 15"
        ),
        "{stderr}"
    );
}

#[test]
fn function_growing_past_its_exception_fails() {
    let (passed, stderr) = health("exception_grows", &[]);
    assert!(!passed);
    assert!(
        stderr.contains("over its health.toml exception of 16"),
        "{stderr}"
    );
}

#[test]
fn exception_raised_since_the_base_fails() {
    let (passed, stderr) = health("exception_raised", &[]);
    assert!(passed, "the code matches the raised exception: {stderr}");

    let base = fixture("exception_raised").join("base.toml");
    let (passed, stderr) = health("exception_raised", &["--base", base.to_str().unwrap()]);
    assert!(!passed);
    assert!(stderr.contains("exception grew from 16 to 17"), "{stderr}");
}

#[test]
fn unsafe_block_without_safety_comment_fails() {
    let (passed, stderr) = health("unsafe_without_safety", &[]);
    assert!(!passed);
    assert!(
        stderr.contains("crates/demo/src/ffi.rs:6: `unsafe` without a `// SAFETY:` comment"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("outside the modules allowlisted"),
        "{stderr}"
    );
}

#[test]
fn unsafe_outside_allowlisted_modules_fails() {
    let (passed, stderr) = health("unsafe_not_allowlisted", &[]);
    assert!(!passed);
    assert!(
        stderr.contains("crates/demo/src/lib.rs:7: `unsafe` outside the modules allowlisted"),
        "{stderr}"
    );
    assert!(!stderr.contains("SAFETY"), "{stderr}");
}
