//! The NixOS module (spec: distribution-and-update, NixOS and Home Manager
//! modules) in its NixOS VM test (`nix/tests/nixos.nix`): a Nix-owned key
//! is read-only, the install is owned by Nix, and a configuration the
//! daemon rejects with exit 78 is not restarted.
//!
//! It needs Nix with flakes and a static Linux binary of this checkout in
//! `PMT_TEST_NIX_BINARY` (a NixOS guest has no generic dynamic loader).
//! Without them it skips with a notice, unless `PMT_TEST_NIX=require` (the
//! CI `channels` job), which makes that a failure.

use std::path::PathBuf;
use std::process::Command;

fn nix_system() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

#[test]
fn the_nixos_module_vm_test_passes() {
    let required = std::env::var("PMT_TEST_NIX").as_deref() == Ok("require");
    let nix = Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let binary = std::env::var_os("PMT_TEST_NIX_BINARY").map(PathBuf::from);
    let Some(binary) = binary.filter(|_| nix) else {
        assert!(
            !required,
            "PMT_TEST_NIX=require but nix or PMT_TEST_NIX_BINARY is missing"
        );
        eprintln!("skipping the NixOS module test: needs nix and PMT_TEST_NIX_BINARY");
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("nix")
        .current_dir(&root)
        .env("PROMETHEUS_TEST_BINARY", &binary)
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            "--impure",
            "--no-link",
            "-L",
            &format!(".#checks.{}.nixos-module", nix_system()),
        ])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{log}");
    for subtest in [
        "Nix-owned key is read-only",
        "the install is owned by Nix",
        "Bad config does not restart-loop",
    ] {
        assert!(
            log.contains(&format!("(finished: subtest: {subtest}")),
            "{subtest}:\n{log}"
        );
    }
}
