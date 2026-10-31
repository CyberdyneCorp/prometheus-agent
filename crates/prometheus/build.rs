//! Bakes the build identity into the binary so `--version` and `version
//! --json` name the exact build without reading anything at runtime: target
//! triple, commit, build profile, build date and `build_id`.
//!
//! `build_id` is a hash of everything that distinguishes one build from
//! another (version, commit, target, profile, enabled surfaces, compiler and
//! build date). The build script reruns whenever any workspace source
//! changes, so every rebuild of a changed tree gets a new build date and
//! therefore a new `build_id`, which is what the daemon compares for
//! stale-build detection. With `SOURCE_DATE_EPOCH` set (release builds) the
//! date is fixed, so rebuilding the same commit reproduces the same id.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

fn main() {
    let target = env_or("TARGET", "unknown");
    let profile = env_or("PROFILE", "unknown");
    let commit = git_commit();
    let date = build_date();
    let id = build_id(&[
        env!("CARGO_PKG_VERSION"),
        &commit,
        &target,
        &profile,
        &features(),
        &rustc_version(),
        &date,
    ]);
    println!("cargo:rustc-env=PMT_BUILD_TARGET={target}");
    println!("cargo:rustc-env=PMT_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=PMT_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=PMT_BUILD_DATE={date}");
    println!("cargo:rustc-env=PMT_BUILD_ID={id}");
    watch_inputs();
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

/// Rerun when HEAD moves (so the commit stays true), when any workspace
/// source or the lockfile changes (so `build_id` changes), and when the
/// reproducible-build date changes.
fn watch_inputs() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let manifest_dir = PathBuf::from(env_or("CARGO_MANIFEST_DIR", "."));
    if let Some(workspace) = manifest_dir.parent().and_then(Path::parent) {
        for input in ["crates", "Cargo.lock"] {
            println!("cargo:rerun-if-changed={}", workspace.join(input).display());
        }
    }
    // Git resolves the paths (a worktree or submodule has a `.git` file, not
    // a directory), and only existing ones are watched: cargo treats a
    // missing path as always changed and would rebuild on every invocation.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git_path(name).filter(|path| path.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// UTC build time in RFC 3339, from `SOURCE_DATE_EPOCH` when set.
fn build_date() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let timestamp = match epoch {
        Some(secs) => jiff::Timestamp::from_second(secs).expect("SOURCE_DATE_EPOCH in range"),
        None => jiff::Timestamp::now(),
    };
    timestamp.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The enabled cargo features, sorted, comma-separated.
fn features() -> String {
    let mut names: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_lowercase))
        .collect();
    names.sort();
    names.join(",")
}

fn rustc_version() -> String {
    let rustc = env_or("RUSTC", "rustc");
    Command::new(rustc)
        .arg("-vV")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_default()
}

/// First 128 bits of SHA-256 over the parts, as lowercase hex.
fn build_id(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize()[..16]
        .iter()
        .fold(String::with_capacity(32), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
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
