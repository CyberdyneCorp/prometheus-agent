//! `prometheus grants list|revoke` (spec: security-boundaries, Grants are
//! stored, reviewable and revocable).

mod common;

use common::{ok, run, stderr, temp_home};
use pmt_config::grants::{GrantsFile, SessionGrants, Subject};
use pmt_core::capability::{Capability, CapabilitySet};

fn plugin() -> Subject {
    "plugin:p@sha256:4f2a".parse().unwrap()
}

fn grants_file(home: &std::path::Path) -> GrantsFile {
    GrantsFile::at(home.join("profiles/default/grants.toml"))
}

/// Spec scenario "Revocation mid-session": a running session's next check
/// after `prometheus grants revoke` fails with `capability not granted`.
#[test]
fn revoking_through_the_cli_reaches_a_running_session() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["grants", "list"]));
    let file = grants_file(home);
    let added = file
        .add(
            &plugin(),
            &CapabilitySet::parse(["net.connect:api.example.com"]).unwrap(),
            "cli",
        )
        .unwrap();
    let session = SessionGrants::new(CapabilitySet::new(), file);
    let call = Capability::parse("net.connect:api.example.com").unwrap();
    session.require(&plugin(), &call).unwrap();

    let id = added[0].id.to_string();
    let listed: serde_json::Value =
        serde_json::from_str(&ok(&run(home, &["grants", "list", "--json"]))).unwrap();
    assert_eq!(listed["schema"], "prometheus.grants.v1");
    assert_eq!(listed["grants"][0]["id"], id.as_str());
    assert_eq!(listed["grants"][0]["subject"], "plugin:p@sha256:4f2a");
    assert_eq!(
        listed["grants"][0]["capability"],
        "net.connect:api.example.com"
    );
    assert_eq!(listed["grants"][0]["surface"], "cli");

    let out = run(home, &["grants", "revoke", &id]);
    ok(&out);
    assert!(
        stderr(&out).contains(&format!("revoked {id}")),
        "{}",
        stderr(&out)
    );

    let err = session.require(&plugin(), &call).unwrap_err();
    assert_eq!(
        err.message(),
        "capability not granted: net.connect:api.example.com"
    );
    assert!(ok(&run(home, &["grants", "list"])).is_empty());
}

#[test]
fn list_filters_by_subject_and_shows_approvals() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["grants", "list"]));
    let file = grants_file(home);
    let caps = CapabilitySet::parse(["llm.call"]).unwrap();
    file.add(&plugin(), &caps, "tui").unwrap();
    file.add(&Subject::Mcp("github".into()), &caps, "cli")
        .unwrap();
    file.allow_command("npm run test*").unwrap();

    let all = ok(&run(home, &["grants", "list"]));
    assert_eq!(all.lines().count(), 3, "{all}");
    assert!(all.contains("approval  npm run test*"), "{all}");
    let only = ok(&run(home, &["grants", "list", "--subject", "mcp:github"]));
    assert_eq!(only.lines().count(), 1, "{only}");
    assert!(only.contains("mcp:github  llm.call  cli"), "{only}");

    let bad = run(home, &["grants", "list", "--subject", "user:me"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn revoking_an_unknown_id_exits_66() {
    let home = temp_home();
    let home = home.path();
    let id = pmt_core::id::GrantId::new().to_string();
    let out = run(home, &["grants", "revoke", &id]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    let malformed = run(home, &["grants", "revoke", "nope"]);
    assert_eq!(malformed.status.code(), Some(65));
}
