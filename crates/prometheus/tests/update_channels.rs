//! Package-manager and image channels (spec: distribution-and-update,
//! Package-manager and image channels; Uniform refusal for installs owned
//! elsewhere): each channel stamps its receipt with its own owner through
//! `prometheus install-receipt` (`WinGet`, which runs no scripts, by its
//! install location), and `prometheus update` refuses every one of them
//! with exit 69 before any request.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::release::{Fixture, code, next_patch};
use pmt_update::Channel;

/// Runs `binary` with a clean environment and no home flag, as a package
/// build does (it must not create a Prometheus home).
fn package_step(binary: &Path, user_home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(binary);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    cmd.env("HOME", user_home)
        .env("USERPROFILE", user_home)
        .env("SOURCE_DATE_EPOCH", "1700000000")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// A package layout: the binary at `relative` under a fresh directory.
fn packaged(f: &Fixture, relative: &str) -> PathBuf {
    let binary = f.dir.path().join(relative);
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::copy(&f.binary, &binary).unwrap();
    binary
}

fn run_installed(f: &Fixture, binary: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(binary);
    common::isolate_user_home(&mut cmd, &f.home);
    cmd.arg("--home").arg(&f.home).args(args);
    cmd.stdin(Stdio::null()).output().unwrap()
}

/// Asserts `update` and `update --check` are refused for `owner`, naming
/// how the install is updated, with a refused receipt and no request.
fn assert_refused(f: &Fixture, binary: &Path, owner: &str, update_with: &str) {
    let before = std::fs::read(binary).unwrap();
    for args in [&["update"][..], &["update", "--check"]] {
        let out = run_installed(f, binary, args);
        let stderr = common::stderr(&out);
        assert_eq!(code(&out), 69, "{owner} {args:?}: {stderr}");
        assert!(stderr.contains(&format!("owner: {owner}")), "{stderr}");
        assert!(
            stderr.contains(&format!("update with: {update_with}")),
            "{stderr}"
        );
        assert_eq!(f.latest_receipt()["outcome"], "refused");
    }
    assert_eq!(std::fs::read(binary).unwrap(), before, "binary unchanged");
    assert!(f.requests().is_empty(), "no request for {owner}");
    let status = common::ok(&run_installed(f, binary, &["update", "--status"]));
    assert!(status.contains(&format!("owner: {owner}\n")), "{status}");
    assert!(
        status.contains(&format!("update with: {update_with}")),
        "{status}"
    );
}

#[test]
fn every_package_channel_stamps_its_owner_and_is_refused_with_69() {
    let channels = [
        (
            "homebrew",
            "Cellar/prometheus/1.4.0/libexec/prometheus",
            "brew upgrade prometheus",
        ),
        (
            "nix",
            "store/abc-prometheus/libexec/prometheus",
            "your Nix configuration",
        ),
        (
            "termux",
            "usr/libexec/prometheus/prometheus",
            "pkg upgrade prometheus",
        ),
        (
            "image",
            "usr/local/bin/prometheus",
            "pull the new image and recreate the container",
        ),
    ];
    for (owner, layout, update_with) in channels {
        let f = Fixture::new(None);
        f.publish(&next_patch(f.version), Channel::Stable, b"unused", None);
        let binary = packaged(&f, &format!("{layout}{}", std::env::consts::EXE_SUFFIX));
        let user_home = f.dir.path().join("build-home");
        std::fs::create_dir_all(&user_home).unwrap();
        let out = package_step(&binary, &user_home, &["install-receipt", "--owner", owner]);
        let written = PathBuf::from(common::ok(&out).trim());
        assert_eq!(
            written,
            std::fs::canonicalize(binary.with_file_name("install.json")).unwrap()
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
        assert_eq!(receipt["owner"], owner);
        assert_eq!(receipt["channel"], "stable");
        assert_eq!(receipt["version"], f.version.to_string());
        assert_eq!(receipt["installed_at"], "2023-11-14T22:13:20Z");
        assert!(
            std::fs::read_dir(&user_home).unwrap().next().is_none(),
            "a package build grows no home"
        );
        assert_refused(&f, &binary, owner, update_with);
    }
}

#[test]
fn homebrew_install_is_owned_by_homebrew() {
    let f = Fixture::new(None);
    let binary = packaged(&f, "Cellar/prometheus/1.4.0/libexec/prometheus");
    common::ok(&package_step(
        &binary,
        f.dir.path(),
        &["install-receipt", "--owner", "homebrew"],
    ));
    let status = common::ok(&run_installed(&f, &binary, &["update", "--status"]));
    assert!(status.contains("owner: homebrew\n"), "{status}");
    assert!(
        status.contains("update with: brew upgrade prometheus\n"),
        "{status}"
    );
}

#[test]
fn a_winget_portable_install_is_recognised_without_a_receipt() {
    let f = Fixture::new(None);
    f.publish(&next_patch(f.version), Channel::Stable, b"unused", None);
    let binary = packaged(
        &f,
        &format!(
            "AppData/Local/Microsoft/WinGet/Packages/CyberdyneCorp.Prometheus_x/prometheus{}",
            std::env::consts::EXE_SUFFIX
        ),
    );
    assert!(!binary.with_file_name("install.json").exists());
    assert_refused(
        &f,
        &binary,
        "winget",
        "winget upgrade CyberdyneCorp.Prometheus",
    );
}

#[test]
fn install_receipt_refuses_self_and_unknown_owners() {
    let f = Fixture::new(None);
    for owner in ["self", "dev", "apt-get"] {
        let out = package_step(
            &f.binary,
            f.dir.path(),
            &["install-receipt", "--owner", owner],
        );
        assert_eq!(code(&out), 2, "{owner}");
        assert!(
            common::stderr(&out).contains("--owner must name a package channel"),
            "{}",
            common::stderr(&out)
        );
    }
    assert!(!f.binary.with_file_name("install.json").exists());
}

/// Scenario: Store install refuses self-update. Scenario: CLI and
/// desktop cannot skew (distribution-and-update, Desktop packages and
/// signing): every desktop package stamps its bundled binary's receipt
/// with its owner, `prometheus update` through it refuses with exit 69
/// naming how that package is updated, and the CLI on `PATH` is the
/// bundled binary, so it reports the package's version.
#[test]
fn desktop_packages_stamp_their_owner_and_the_cli_is_the_bundled_binary() {
    let packages = [
        (
            "desktop",
            "Prometheus.app/Contents/MacOS/prometheus",
            "the desktop app's updater",
        ),
        (
            "msix",
            "WindowsApps/CyberdyneCorp.Prometheus_x64/prometheus",
            "App Installer",
        ),
        (
            "msstore",
            "WindowsApps/CyberdyneCorp.PrometheusStore_x64/prometheus",
            "Microsoft Store",
        ),
        (
            "system-package",
            "usr/lib/prometheus-desktop/prometheus",
            "the system package manager",
        ),
    ];
    for (owner, layout, update_with) in packages {
        let f = Fixture::new(None);
        f.publish(&next_patch(f.version), Channel::Stable, b"unused", None);
        let binary = packaged(&f, &format!("{layout}{}", std::env::consts::EXE_SUFFIX));
        let receipt_path = pmt_update::install::beside(&binary);
        // An app bundle keeps it in Contents/Resources, which every bundle has.
        std::fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
        let user_home = f.dir.path().join("build-home");
        std::fs::create_dir_all(&user_home).unwrap();
        common::ok(&package_step(
            &binary,
            &user_home,
            &["install-receipt", "--owner", owner],
        ));
        assert_refused(&f, &binary, owner, update_with);
        let version: serde_json::Value = serde_json::from_str(&common::ok(&run_installed(
            &f,
            &binary,
            &["version", "--json"],
        )))
        .unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        // `v1.6.0` in the receipt, `1.6.0` in `version --json`.
        let stamped = receipt["version"].as_str().unwrap().trim_start_matches('v');
        assert_eq!(version["version"], stamped, "{owner}");
    }
}
