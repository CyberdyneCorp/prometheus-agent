//! `prometheus secrets bitwarden|onepassword` against stub `bws` and `op`
//! CLIs in the managed runtime store (spec: configuration-and-profiles,
//! External secret source commands).
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use common::{ok, run, run_with_stdin, stderr, stdout, temp_home};

/// A stub `bws`: accepts the tokens `good-token` and `new-token`, lists
/// `OPENAI_API_KEY` and `EXA_API_KEY`.
const BWS: &str = r#"#!/bin/sh
[ "$1" = "--version" ] && { echo "bws 9.9.9"; exit 0; }
case "$BWS_ACCESS_TOKEN" in good-token|new-token) ;; *) exit 1 ;; esac
case "$1" in
  project) echo '[{"id":"p1","name":"main"}]' ;;
  secret) echo '[{"key":"OPENAI_API_KEY","value":"sk-bw-1"},{"key":"EXA_API_KEY","value":"exa-bw"}]' ;;
esac
"#;

/// A stub `op`: accepts `op-token`, one vault `Agents` with two items.
const OP: &str = r#"#!/bin/sh
[ "$1" = "--version" ] && { echo "2.99.0"; exit 0; }
[ "$OP_SERVICE_ACCOUNT_TOKEN" = "op-token" ] || exit 1
case "$1 $2" in
  "vault list") echo '[{"id":"v1","name":"Agents"}]' ;;
  "item list") echo '[{"id":"i1","title":"GITHUB_TOKEN"},{"id":"i2","title":"not a name"}]' ;;
esac
"#;

fn stub(home: &Path, name: &str, script: &str) {
    let dir = home.join("runtimes").join(name).join("9.9.9-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn secrets_file(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/secrets.toml")).unwrap_or_default()
}

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap_or_default()
}

#[test]
fn invalid_rotated_token_refused() {
    let home = temp_home();
    stub(home.path(), "bws", BWS);
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "bitwarden", "setup", "--project-id", "p1"],
        "good-token\n",
    ));
    assert!(secrets_file(home.path()).contains("good-token"));
    assert!(config(home.path()).contains("project_id = \"p1\""));

    let out = run_with_stdin(
        home.path(),
        &["secrets", "bitwarden", "token"],
        "typo-token\n",
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("unchanged"), "{}", stderr(&out));
    let stored = secrets_file(home.path());
    assert!(stored.contains("good-token") && !stored.contains("typo-token"));

    ok(&run_with_stdin(
        home.path(),
        &["secrets", "bitwarden", "token"],
        "new-token\n",
    ));
    assert!(secrets_file(home.path()).contains("new-token"));
}

#[test]
fn bitwarden_status_shows_names_and_sync_writes_only_with_apply() {
    let home = temp_home();
    stub(home.path(), "bws", BWS);
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "bitwarden", "setup"],
        "good-token\n",
    ));
    let status = ok(&run(home.path(), &["secrets", "bitwarden", "status"]));
    assert!(
        status.contains("bws 9.9.9") && status.contains("token: valid"),
        "{status}"
    );
    assert!(
        status.contains("EXA_API_KEY") && !status.contains("sk-bw-1"),
        "{status}"
    );

    // A stored OPENAI_API_KEY shadows the project's.
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-local\n",
    ));
    let preview = run(home.path(), &["secrets", "bitwarden", "sync"]);
    let text = ok(&preview);
    assert!(text.contains("EXA_API_KEY  added"), "{text}");
    assert!(text.contains("OPENAI_API_KEY  shadowed"), "{text}");
    assert!(!config(home.path()).contains("names"));

    ok(&run(
        home.path(),
        &["secrets", "bitwarden", "sync", "--apply"],
    ));
    assert!(
        config(home.path()).contains("names = [\"EXA_API_KEY\", \"OPENAI_API_KEY\"]"),
        "{}",
        config(home.path())
    );
    ok(&run(home.path(), &["secrets", "bitwarden", "disable"]));
    assert!(config(home.path()).contains("enabled = false"));
}

#[test]
fn setup_without_the_cli_asks_before_installing() {
    let home = temp_home();
    let out = run(home.path(), &["secrets", "bitwarden", "setup"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("install bws 2.1.0"),
        "{}",
        stderr(&out)
    );
    assert!(!home.path().join("runtimes/bws").exists());
}

#[test]
fn onepassword_setup_map_and_sync() {
    let home = temp_home();
    stub(home.path(), "op", OP);
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "onepassword", "setup", "--vault", "Agents"],
        "op-token\n",
    ));
    assert!(config(home.path()).contains("vault = \"Agents\""));
    ok(&run(
        home.path(),
        &[
            "secrets",
            "onepassword",
            "map",
            "OPENAI_API_KEY",
            "op://Agents/OpenAI/credential",
        ],
    ));
    let status = ok(&run(home.path(), &["secrets", "onepassword", "status"]));
    assert!(
        status.contains("mapped: OPENAI_API_KEY") && status.contains("token: valid"),
        "{status}"
    );

    let preview = ok(&run(home.path(), &["secrets", "onepassword", "sync"]));
    assert!(
        preview.contains("GITHUB_TOKEN  added  op://Agents/GITHUB_TOKEN/credential"),
        "{preview}"
    );
    assert!(!preview.contains("not a name"));
    assert!(!config(home.path()).contains("GITHUB_TOKEN"));
    ok(&run(
        home.path(),
        &["secrets", "onepassword", "sync", "--apply"],
    ));
    assert!(config(home.path()).contains("GITHUB_TOKEN = \"op://Agents/GITHUB_TOKEN/credential\""));

    ok(&run(
        home.path(),
        &["secrets", "onepassword", "unmap", "OPENAI_API_KEY"],
    ));
    assert!(!config(home.path()).contains("OpenAI"));
    let bad = run_with_stdin(
        home.path(),
        &["secrets", "onepassword", "setup", "--vault", "Agents"],
        "wrong\n",
    );
    assert_eq!(bad.status.code(), Some(78), "{}", stdout(&bad));
    assert!(secrets_file(home.path()).contains("op-token"));
}
