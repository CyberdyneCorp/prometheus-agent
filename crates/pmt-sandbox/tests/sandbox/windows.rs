//! The AppContainer + Job Object backend on a real Windows host
//! (spec: security-boundaries, OS sandbox per platform).

use std::path::Path;
use std::process::Output;

use pmt_sandbox::{Feature, Level, Sandbox};

use crate::common::{Fixture, SECRET, confine};

/// Runs `cmd /C <line>` confined, from `cwd` (a directory the container may
/// use, as the terminal backend always sets).
fn cmd(sandbox: &Sandbox, cwd: &Path, line: &[&str]) -> Output {
    let mut command = sandbox.command("cmd.exe").unwrap();
    command.current_dir(cwd).arg("/C").args(line);
    command.output().unwrap()
}

fn write(sandbox: &Sandbox, cwd: &Path, path: &Path) -> Output {
    cmd(sandbox, cwd, &["echo", "x>", path.to_str().unwrap()])
}

#[test]
fn writes_outside_the_workspace_fail() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let inside = fixture.work.join("made-here.txt");
    let output = write(&sandbox, &fixture.work, &inside);
    assert!(output.status.success(), "{output:?}");
    assert!(inside.exists());

    let outside = fixture.outside.join("escaped.txt");
    let output = write(&sandbox, &fixture.work, &outside);
    assert!(!output.status.success(), "{output:?}");
    assert!(!outside.exists());
}

#[test]
fn credential_stores_stay_denied_under_a_broad_read_grant() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&["fs.read:~/**"]));
    let notes = fixture.home.join("notes.txt");
    let output = cmd(&sandbox, &fixture.work, &["type", notes.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    for store in [".ssh\\id_rsa", ".aws\\credentials"] {
        let path = fixture.home.join(store);
        let output = cmd(&sandbox, &fixture.work, &["type", path.to_str().unwrap()]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    }
}

#[test]
fn report_strength_per_feature() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "appcontainer");
    let levels: Vec<(Feature, Level)> = report
        .features
        .iter()
        .map(|f| (f.feature, f.level))
        .collect();
    assert_eq!(
        levels,
        [
            (Feature::Fs, Level::Enforced),
            (Feature::Net, Level::Enforced),
            (Feature::Process, Level::Enforced),
            (Feature::Syscalls, Level::Partial),
        ]
    );
    assert_eq!(report.enforcement().to_string(), "full");
}

fn acl(path: &Path) -> String {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn acl_entries_are_revoked_when_the_sandbox_ends() {
    let fixture = Fixture::new();
    let before = acl(&fixture.work);
    let sandbox = confine(fixture.policy(&[]));
    // AppContainer SIDs start with S-1-15-2-.
    assert!(
        acl(&fixture.work).contains("S-1-15-2-"),
        "{}",
        acl(&fixture.work)
    );
    drop(sandbox);
    assert_eq!(acl(&fixture.work), before);
}
