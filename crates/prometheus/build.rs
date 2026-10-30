//! Bakes the build identity (target triple and commit) into the binary so
//! `--version` can name the exact build without reading anything at runtime.

use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=PMT_BUILD_TARGET={target}");
    println!("cargo:rustc-env=PMT_BUILD_COMMIT={}", git_commit());
    // Rebuild when HEAD moves so the commit in the identity line stays true.
    for path in ["../../.git/HEAD", "../../.git/refs/heads"] {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// Short commit hash, or `unknown` when building outside a git checkout
/// (for example from a source tarball).
fn git_commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}
