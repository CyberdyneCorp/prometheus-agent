//! Test support (feature `testkit`): the path of the `pmt-mcp-fixture`
//! binary, built on demand.
//!
//! A workspace test run (`--all-features`) already builds the fixture next
//! to the test binaries; when it is missing (a run of one crate's tests),
//! it is built once with `cargo build -p pmt-mcp --features fixture` into
//! the same target directory.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// The fixture binary's name.
pub const FIXTURE: &str = "pmt-mcp-fixture";

/// The path of the fixture binary.
///
/// # Panics
///
/// When it is missing and cannot be built.
#[must_use]
pub fn fixture() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(build).clone()
}

fn build() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    // target/<profile>/deps/<test> or target/<profile>/<binary>.
    let mut dir = exe.parent().expect("executable directory").to_path_buf();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join(format!("{FIXTURE}{}", std::env::consts::EXE_SUFFIX));
    if path.exists() {
        return path;
    }
    let target = dir.parent().expect("target directory");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let profile = dir.file_name().and_then(|n| n.to_str()).unwrap_or("debug");
    let mut command = Command::new(cargo);
    command
        .args([
            "build",
            "-p",
            "pmt-mcp",
            "--bin",
            FIXTURE,
            "--features",
            "fixture",
        ])
        .arg("--target-dir")
        .arg(target);
    if profile != "debug" {
        command.args(["--profile", profile]);
    }
    let status = command.status().expect("cargo build of the MCP fixture");
    assert!(status.success(), "building {FIXTURE} failed");
    path
}
