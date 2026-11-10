//! Test support (feature `testkit`): the path of the `pmt-llama-stub`
//! binary, a stand-in `llama-server`, built on demand.
//!
//! A workspace test run (`--all-features`) already builds it next to the
//! test binaries; when it is missing (a run of one crate's tests) it is
//! built once with `cargo build -p pmt-local --features stub` into the same
//! target directory.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// The stub binary's name.
pub const STUB: &str = "pmt-llama-stub";

/// The path of the stub binary.
///
/// # Panics
///
/// When it is missing and cannot be built.
#[must_use]
pub fn stub() -> PathBuf {
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
    let path = dir.join(format!("{STUB}{}", std::env::consts::EXE_SUFFIX));
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
            "pmt-local",
            "--bin",
            STUB,
            "--features",
            "stub",
        ])
        .arg("--target-dir")
        .arg(target);
    if profile != "debug" {
        command.args(["--profile", profile]);
    }
    let status = command
        .status()
        .expect("cargo build of the llama-server stub");
    assert!(status.success(), "building {STUB} failed");
    path
}
