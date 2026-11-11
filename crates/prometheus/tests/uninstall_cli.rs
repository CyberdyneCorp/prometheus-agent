//! `prometheus uninstall` (spec: distribution-and-update, Uninstall;
//! Uninstall scopes and dry run) on a copy of the built binary
//! "installed" with a receipt.

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::release::{Fixture, code};

/// Runs the installed copy with the service-manager test seam.
fn run(f: &Fixture, seam: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(&f.binary);
    common::isolate_user_home(&mut cmd, &f.home);
    cmd.env("PMT_TEST_SERVICE_DIR", seam)
        .arg("--home")
        .arg(&f.home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The `pmt` alias and a `prometheus.prev` beside the binary.
fn alias_and_prev(f: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    let alias = f
        .binary
        .with_file_name(format!("pmt{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&f.binary, &alias).unwrap();
    let prev = f.prev();
    std::fs::write(&prev, b"previous build").unwrap();
    (alias, prev)
}

#[test]
fn data_is_kept_by_default() {
    let f = Fixture::self_install();
    let seam = f.dir.path().join("seam");
    let (alias, prev) = alias_and_prev(&f);
    let out = run(&f, &seam, &["uninstall"]);
    assert_eq!(code(&out), 0, "{}", common::stderr(&out));
    for gone in [
        &f.binary,
        &alias,
        &prev,
        &f.binary.with_file_name("install.json"),
    ] {
        assert!(!gone.exists(), "{} removed", gone.display());
    }
    assert!(f.home.join("profiles").is_dir(), "the home is kept");
    assert!(
        common::stderr(&out).contains("`prometheus uninstall --purge` removes it"),
        "{}",
        common::stderr(&out)
    );
}

#[cfg(all(
    feature = "gateway",
    any(target_os = "linux", target_os = "macos", windows)
))]
#[test]
fn dry_run_lists_the_service_unit() {
    let f = Fixture::self_install();
    let seam = f.dir.path().join("seam");
    std::fs::create_dir_all(&seam).unwrap();
    let (_, prev) = alias_and_prev(&f);
    common::ok(&run(
        &f,
        &seam,
        &["daemon", "install-service", "--no-start"],
    ));
    // The systemd user unit or launchd agent under the seam's user home;
    // on Windows the Scheduled Task (and its wrapper in the home).
    let unit = common::files_under(&seam.join("home"))
        .into_iter()
        .find(|p| p.is_file());
    let out = run(&f, &seam, &["uninstall", "--purge", "--dry-run"]);
    let text = common::ok(&out);
    for listed in [&f.binary, &prev, &f.home] {
        assert!(
            text.contains(&listed.display().to_string()),
            "{} not listed:\n{text}",
            listed.display()
        );
    }
    match &unit {
        Some(unit) => assert!(
            text.contains(&format!("service unit: {}", unit.display())),
            "{text}"
        ),
        None => assert!(
            cfg!(windows) && text.contains("service unit: Scheduled Task"),
            "{text}"
        ),
    }
    assert!(text.contains("nothing was changed (dry run)"), "{text}");
    assert!(f.binary.exists() && prev.exists() && f.home.exists());
    assert!(unit.is_none_or(|u| u.exists()), "the unit is kept");
}

#[test]
fn a_data_wipe_on_a_homebrew_install_keeps_the_binary() {
    let f = Fixture::new(Some("homebrew"));
    let seam = f.dir.path().join("seam");
    common::ok(&f.run(&["config", "set", "model.default", "mock"]));
    let out = run(&f, &seam, &["uninstall"]);
    assert_eq!(code(&out), 69, "{}", common::stderr(&out));
    assert!(
        common::stderr(&out).contains("brew uninstall prometheus"),
        "{}",
        common::stderr(&out)
    );
    let out = run(&f, &seam, &["uninstall", "--data-only", "--yes"]);
    assert_eq!(code(&out), 0, "{}", common::stderr(&out));
    assert!(
        !f.home.join("profiles").exists(),
        "the home's contents are gone"
    );
    assert!(!f.home.join("daemon.toml").exists());
    assert!(f.binary.exists(), "the binary remains");
}

#[test]
fn purging_without_a_terminal_needs_yes() {
    let f = Fixture::self_install();
    let seam = f.dir.path().join("seam");
    let out = run(&f, &seam, &["uninstall", "--purge"]);
    assert_eq!(code(&out), 4, "{}", common::stderr(&out));
    assert!(
        common::stderr(&out).contains("rerun with --yes"),
        "{}",
        common::stderr(&out)
    );
    assert!(f.binary.exists() && f.home.join("profiles").exists());
}
