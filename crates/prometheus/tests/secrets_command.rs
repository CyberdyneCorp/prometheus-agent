//! The command secret source through the binary (spec:
//! security-boundaries, Command secret source).

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{ok, run, stderr, temp_home};

/// A home whose `secrets.command` runs `script` (written outside the home,
/// so nothing it prints can be found in the home by accident).
fn home_with_helper(script: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let home = temp_home();
    ok(&run(home.path(), &["secrets", "list"]));
    let tools = temp_home();
    let helper = tools.path().join("helper.sh");
    std::fs::write(&helper, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    common::append_config(
        home.path(),
        &format!(
            "\n[secrets.command]\nenabled = true\nargv = [\"{}\"]\n",
            helper.display()
        ),
    );
    (home, tools)
}

fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            hits.extend(files_containing(&path, needle));
        } else if std::fs::read(&path)
            .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        {
            hits.push(path.display().to_string());
        }
    }
    hits
}

/// Scenario: Helper times out.
#[test]
fn a_helper_waiting_for_a_pin_is_killed_and_startup_continues() {
    let (home, _tools) =
        home_with_helper("printf 'PIN-PROMPT-7731: ' >&2; printf 'PIN-PROMPT-7731'; sleep 30");
    let started = Instant::now();
    let out = run(home.path(), &["secrets", "list"]);
    let took = started.elapsed();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        took >= Duration::from_secs(3) && took < Duration::from_secs(15),
        "{took:?}"
    );
    let err = stderr(&out);
    assert!(
        err.contains("helper command failed (timed out after 3 s; killed)"),
        "{err}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!err.contains("PIN-PROMPT") && !stdout.contains("PIN-PROMPT"));
    assert_eq!(
        files_containing(home.path(), "PIN-PROMPT"),
        Vec::<String>::new()
    );
}

/// A helper's values are listed with their source and resolve like other
/// secrets; output that is not a map contributes nothing.
#[test]
fn helper_values_are_secrets_and_bad_output_is_reported() {
    let (home, _tools) = home_with_helper("echo HELPER_TOKEN=value-from-helper");
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(listed.contains("HELPER_TOKEN  command"), "{listed}");
    let (home, _tools) = home_with_helper("echo 'not a map'");
    let out = run(home.path(), &["secrets", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("helper output was not a NAME=VALUE map"),
        "{}",
        stderr(&out)
    );
}
