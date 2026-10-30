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

/// A hard-linked `pmt` next to the built binary, the same shape `scripts/dev-install` creates.
fn pmt_alias() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("alias");
    std::fs::create_dir_all(&dir).expect("alias dir");
    let alias = dir.join(format!("pmt{}", std::env::consts::EXE_SUFFIX));
    let _ = std::fs::remove_file(&alias);
    std::fs::hard_link(BIN, &alias)
        .or_else(|_| std::fs::copy(BIN, &alias).map(drop))
        .expect("alias created");
    alias
}

#[test]
fn version_line_has_identity_shape() {
    let line = version_line(Path::new(BIN), "--version");
    let expected_prefix = format!("prometheus {} (", env!("CARGO_PKG_VERSION"));
    assert!(line.starts_with(&expected_prefix), "got {line:?}");
    assert!(line.ends_with(")\n"), "got {line:?}");
    assert_eq!(line.lines().count(), 1, "got {line:?}");
}

#[test]
fn short_flag_matches_long_flag() {
    let exe = Path::new(BIN);
    assert_eq!(version_line(exe, "-V"), version_line(exe, "--version"));
}

#[test]
fn pmt_alias_prints_the_same_identity_line() {
    let alias = pmt_alias();
    assert_eq!(
        version_line(&alias, "--version"),
        version_line(Path::new(BIN), "--version")
    );
}
