//! Bakes the build identity (target triple and commit) into the binary so
//! `--version` can name the exact build without reading anything at runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=PMT_BUILD_TARGET={target}");
    println!("cargo:rustc-env=PMT_BUILD_COMMIT={}", git_commit());
    println!("cargo:rerun-if-changed=build.rs");
    // Rebuild when HEAD moves so the commit in the identity line stays true.
    // Git resolves the paths (a worktree or submodule has a `.git` file, not
    // a directory), and only existing ones are watched: cargo treats a
    // missing path as always changed and would rebuild on every invocation.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git_path(name).filter(|path| path.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Short commit hash, or `unknown` when building outside a git checkout
/// (for example from a source tarball).
fn git_commit() -> String {
    git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned())
}

/// Where git keeps `name` for this checkout, made absolute because git
/// answers relative to the current directory.
fn git_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git(&["rev-parse", "--git-path", name])?);
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    Some(Path::new(&manifest_dir).join(path))
}

/// Trimmed stdout of a successful, non-empty git command.
fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
