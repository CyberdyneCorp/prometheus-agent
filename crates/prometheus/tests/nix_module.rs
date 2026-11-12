//! The NixOS and Home Manager modules (spec: distribution-and-update, NixOS
//! and Home Manager modules) in their NixOS VM test (`nix/tests/nixos.nix`):
//! a Nix-owned key is read-only, the install is owned by Nix, and a
//! configuration the daemon rejects with exit 78 is not restarted, by the
//! native unit, the NixOS container unit and the Home Manager container
//! unit; and in the Home Manager evaluation test
//! (`nix/tests/home-manager.nix`: native and container services on Linux
//! and macOS).
//!
//! They need Nix with flakes, and the VM test a static Linux binary of this
//! checkout in `PMT_TEST_NIX_BINARY` (a NixOS guest has no generic dynamic
//! loader). Without them they skip with a notice, unless
//! `PMT_TEST_NIX=require` (the CI `channels` job), which makes that a
//! failure.

use std::path::PathBuf;
use std::process::Command;

fn nix_system() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

fn required() -> bool {
    std::env::var("PMT_TEST_NIX").as_deref() == Ok("require")
}

fn nix_available() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `nix build` of a flake check of this checkout; its build log.
fn build_check(check: &str, binary: Option<&PathBuf>) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new("nix");
    command.current_dir(&root).args([
        "--extra-experimental-features",
        "nix-command flakes",
        "build",
        "--no-link",
        "-L",
    ]);
    if let Some(binary) = binary {
        command
            .env("PROMETHEUS_TEST_BINARY", binary)
            .arg("--impure");
    }
    let out = command
        .arg(format!(".#checks.{}.{check}", nix_system()))
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{log}");
    log
}

#[test]
fn the_home_manager_module_evaluation_test_passes() {
    if !nix_available() {
        assert!(!required(), "PMT_TEST_NIX=require but nix is missing");
        eprintln!("skipping the Home Manager module test: needs nix");
        return;
    }
    build_check("home-manager-module", None);
}

#[test]
fn the_nixos_module_vm_test_passes() {
    let required = required();
    let nix = nix_available();
    let binary = std::env::var_os("PMT_TEST_NIX_BINARY").map(PathBuf::from);
    let Some(binary) = binary.filter(|_| nix) else {
        assert!(
            !required,
            "PMT_TEST_NIX=require but nix or PMT_TEST_NIX_BINARY is missing"
        );
        eprintln!("skipping the NixOS module test: needs nix and PMT_TEST_NIX_BINARY");
        return;
    };
    let log = build_check("nixos-module", Some(&binary));
    for subtest in [
        "Nix-owned key is read-only",
        "the install is owned by Nix",
        "Bad config does not restart-loop",
        "Bad config does not restart-loop in a container",
        "Home Manager container unit",
    ] {
        assert!(
            log.contains(&format!("(finished: subtest: {subtest}")),
            "{subtest}:\n{log}"
        );
    }
}
