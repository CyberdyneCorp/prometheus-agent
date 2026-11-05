//! `prometheus vault` through the binary (spec: security-boundaries, Vault
//! management CLI; Credential vault storage).

mod common;

use std::path::Path;

use common::{ok, run, run_with_stdin, stderr, temp_home};
use serde_json::Value;

fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for path in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
        if path.is_dir() {
            hits.extend(files_containing(&path, needle));
        } else if std::fs::read(&path).is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle)) {
            hits.push(path.display().to_string());
        }
    }
    hits
}

/// Scenario: Password on the command line.
#[test]
fn a_password_on_the_command_line_exits_2() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["vault", "add", "--login", "--password", "hunter2"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("secrets are read only from a masked prompt"),
        "{}",
        stderr(&out)
    );
    assert!(
        !home
            .path()
            .join("profiles/default/vault/vault.enc")
            .exists()
    );
}

#[test]
fn items_are_added_listed_as_metadata_and_removed_after_confirmation() {
    let home = temp_home();
    let home = home.path();
    let out = run_with_stdin(
        home,
        &[
            "vault",
            "add",
            "--login",
            "--label",
            "GitHub",
            "--origin",
            "https://github.com/login",
            "--identifier",
            "octocat",
            "--with-totp",
        ],
        "s3cret-vault-pw\nJBSWY3DPEHPK3PXP\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run_with_stdin(
        home,
        &[
            "vault", "add", "--card", "--label", "Visa", "--holder", "Ann Lee", "--expiry", "12/29",
        ],
        "4111111111111111\n123\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "vault", "list"]))).unwrap();
    assert_eq!(listed["schema"], "prometheus.vault_list.v1");
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{listed}");
    let login = items.iter().find(|i| i["kind"] == "login").unwrap();
    assert_eq!(login["origins"][0], "https://github.com");
    assert_eq!(login["identifier"], "octocat");
    assert_eq!(login["totp"], true);
    let text = listed.to_string();
    for secret in ["s3cret-vault-pw", "JBSWY3DPEHPK3PXP", "4111111111111111"] {
        assert!(!text.contains(secret), "{text}");
        assert_eq!(
            files_containing(home, secret),
            Vec::<String>::new(),
            "{secret}"
        );
    }
    let card = items.iter().find(|i| i["kind"] == "card").unwrap();
    let handle = card["handle"].as_str().unwrap();
    assert!(handle.starts_with("card_"), "{handle}");
    let out = run(home, &["vault", "rm", handle]);
    assert_eq!(
        out.status.code(),
        Some(i32::from(pmt_core::exit::APPROVAL_REQUIRED))
    );
    ok(&run(home, &["vault", "rm", handle, "--yes"]));
    let text = ok(&run(home, &["vault", "list"]));
    assert!(text.contains("GitHub") && !text.contains("Visa"), "{text}");
}

#[test]
fn password_manager_sources_are_shown_and_toggled() {
    let home = temp_home();
    let home = home.path();
    let shown = ok(&run(home, &["vault", "sources"]));
    assert!(shown.contains("onepassword  enabled"), "{shown}");
    ok(&run(home, &["vault", "sources", "--disable", "bitwarden"]));
    let shown = ok(&run(home, &["vault", "sources"]));
    assert!(shown.contains("bitwarden  disabled"), "{shown}");
}
