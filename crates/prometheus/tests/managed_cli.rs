//! The administrator-managed layer against the built binary (spec:
//! configuration-and-profiles, Managed configuration layer; Managed secrets
//! and managed visibility). Debug builds read the managed directory from
//! `PMT_TEST_MANAGED_DIR` instead of `/etc/prometheus`.

mod common;

use std::path::Path;
use std::process::Output;

use common::{ok, run_cmd_with_stdin, stderr, stdout, temp_home};

/// A home and a managed directory holding `managed` as `managed.toml`.
fn managed(managed: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let home = temp_home();
    let dir = home.path().join("managed-dir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("managed.toml"), managed).unwrap();
    (home, dir)
}

fn run_managed(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = common::command(home);
    cmd.env("PMT_TEST_MANAGED_DIR", dir).args(args);
    run_cmd_with_stdin(&mut cmd, "")
}

const LOCKED: &str = "locked = [\"approvals.mode\"]\n\n[approvals]\nmode = \"manual\"\n\n[agent]\nmax_iterations = 42\n";

/// Scenario: Locked key refused.
#[test]
fn a_locked_key_is_refused_by_every_writer_with_exit_77() {
    let (home, dir) = managed(LOCKED);
    let message = "config key 'approvals.mode' is managed by your administrator";
    for args in [
        &["config", "set", "approvals.mode", "off"][..],
        &["config", "unset", "approvals.mode"],
        &[
            "--set",
            "approvals.mode=off",
            "config",
            "get",
            "approvals.mode",
        ],
    ] {
        let out = run_managed(home.path(), &dir, args);
        assert_eq!(out.status.code(), Some(77), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(message), "{}", stderr(&out));
    }
    // The managed values win over the profile's own.
    ok(&run_managed(
        home.path(),
        &dir,
        &["config", "set", "agent.max_iterations", "7"],
    ));
    let got = ok(&run_managed(
        home.path(),
        &dir,
        &["config", "get", "agent.max_iterations"],
    ));
    assert_eq!(got.trim(), "42");
    let explained = ok(&run_managed(
        home.path(),
        &dir,
        &["config", "explain", "agent.max_iterations"],
    ));
    assert!(explained.contains("managed.toml"), "{explained}");
}

/// Scenario: Managed key shown.
#[test]
fn config_show_names_the_managed_file_and_its_keys() {
    let (home, dir) = managed(LOCKED);
    let shown = ok(&run_managed(home.path(), &dir, &["config", "show"]));
    let path = dir.join("managed.toml");
    assert!(
        shown.starts_with(&format!(
            "# managed by your administrator: {}\n",
            path.display()
        )),
        "{shown}"
    );
    assert!(shown.contains("#   approvals.mode (locked)\n"), "{shown}");
    assert!(shown.contains("#   agent.max_iterations\n"), "{shown}");
    let doctor = stdout(&run_managed(home.path(), &dir, &["doctor", "config"]));
    assert!(
        doctor.contains(&format!("managed configuration {}: loaded", path.display())),
        "{doctor}"
    );
}

#[test]
fn an_invalid_managed_layer_fails_closed() {
    let (home, dir) = managed("[approvals]\nmode = \"loud\"\n");
    let out = run_managed(home.path(), &dir, &["config", "get", "approvals.mode"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("managed.toml"), "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn managed_secrets_win_and_cannot_be_changed() {
    use std::os::unix::fs::PermissionsExt as _;
    let (home, dir) = managed("");
    let secrets = dir.join("managed-secrets.toml");
    std::fs::write(&secrets, "OPENAI_API_KEY = \"sk-managed-0123456789\"\n").unwrap();
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut set = common::command(home.path());
    set.env("PMT_TEST_MANAGED_DIR", &dir)
        .args(["secrets", "set", "OPENAI_API_KEY"]);
    let out = run_cmd_with_stdin(&mut set, "sk-mine-0123456789");
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("secret 'OPENAI_API_KEY' is managed by your administrator"),
        "{}",
        stderr(&out)
    );
    let mut list = common::command(home.path());
    list.env("PMT_TEST_MANAGED_DIR", &dir)
        .env("PMT_SECRET_OPENAI_API_KEY", "sk-env-0123456789")
        .args(["--json", "secrets", "list"]);
    let listed: serde_json::Value =
        serde_json::from_str(&ok(&run_cmd_with_stdin(&mut list, ""))).unwrap();
    let rows = listed["secrets"].as_array().unwrap();
    assert_eq!(rows[0]["source"], "managed");
    assert_eq!(rows[0]["shadowed"], false);
    assert!(
        rows.iter()
            .any(|r| r["source"] == "env" && r["shadowed"] == true),
        "{listed}"
    );
    let doctor = stdout(&run_managed(home.path(), &dir, &["doctor", "config"]));
    assert!(
        doctor.contains(&format!("managed secrets {}: loaded", secrets.display())),
        "{doctor}"
    );
}
