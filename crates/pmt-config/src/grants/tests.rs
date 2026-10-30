use pmt_core::capability::{Capability, CapabilitySet};

use super::{GrantsFile, SessionGrants, Subject, baseline};
use crate::schema::{CapabilityText, SecurityConfig};

fn file(dir: &tempfile::TempDir) -> GrantsFile {
    GrantsFile::at(dir.path().join("grants.toml"))
}

fn caps(texts: &[&str]) -> CapabilitySet {
    CapabilitySet::parse(texts).unwrap()
}

fn plugin() -> Subject {
    "plugin:tracker@sha256:4f2a".parse().unwrap()
}

#[test]
fn grants_are_recorded_reloaded_and_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let grants = file(&dir);
    assert!(
        grants.load().unwrap().grants.is_empty(),
        "missing file = none"
    );
    let added = grants
        .add(
            &plugin(),
            &caps(&["net.connect:api.example.com", "llm.call"]),
            "cli",
        )
        .unwrap();
    assert_eq!(added.len(), 2);
    let again = grants
        .add(
            &plugin(),
            &caps(&["net.connect:api.example.com:443"]),
            "tui",
        )
        .unwrap();
    assert!(again.is_empty(), "already covered");

    let loaded = grants.load().unwrap();
    assert_eq!(loaded.grants, added);
    assert_eq!(
        loaded.capabilities(&plugin()).to_strings(),
        ["net.connect:api.example.com", "llm.call"]
    );
    assert!(loaded.capabilities(&Subject::Profile).is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(grants.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn revoke_removes_one_grant_and_unknown_ids_are_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let grants = file(&dir);
    let added = grants
        .add(
            &plugin(),
            &caps(&["net.connect:a.example", "net.connect:b.example"]),
            "cli",
        )
        .unwrap();
    let revoked = grants.revoke(added[0].id).unwrap();
    assert_eq!(revoked, added[0]);
    assert_eq!(grants.load().unwrap().grants, [added[1].clone()]);
    let err = grants.revoke(added[0].id).unwrap_err();
    assert_eq!(err.code().code, "grants.not_found");
    assert_eq!(err.exit_code(), 66);
}

#[test]
fn an_unknown_capability_in_the_file_is_a_load_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let grants = file(&dir);
    std::fs::write(
        grants.path(),
        "[[grant]]\nid = \"grant_01JAAAAAAAAAAAAAAAAAAAAAAA\"\nsubject = \"profile\"\n\
         capability = \"filesystem.write:/tmp\"\nsurface = \"cli\"\n\
         granted_at = \"2026-10-30T12:00:00Z\"\n",
    )
    .unwrap();
    let err = grants.load().unwrap_err();
    assert_eq!(err.code().code, "capability.unknown");
    assert!(
        err.message()
            .ends_with("unknown capability 'filesystem.write:/tmp'"),
        "{}",
        err.message()
    );
}

/// tool-system scenario "Hand-removed entry stays removed".
#[test]
fn a_hand_removed_approval_is_never_written_back() {
    let dir = tempfile::tempdir().unwrap();
    let grants = file(&dir);
    assert!(grants.allow_command("npm run test*").unwrap());
    assert!(grants.allow_command("cargo build").unwrap());
    assert!(
        !grants.allow_command("cargo build").unwrap(),
        "no duplicate"
    );
    let text = std::fs::read_to_string(grants.path()).unwrap();
    std::fs::write(grants.path(), text.replace("\"npm run test*\", ", "")).unwrap();
    assert_eq!(grants.load().unwrap().allow, ["cargo build"]);

    grants.allow_command("make").unwrap();
    assert_eq!(grants.load().unwrap().allow, ["cargo build", "make"]);
}

#[cfg(unix)]
#[test]
fn a_symlinked_grants_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("elsewhere.toml");
    std::fs::write(&target, "").unwrap();
    let grants = file(&dir);
    std::os::unix::fs::symlink(&target, grants.path()).unwrap();
    assert_eq!(grants.load().unwrap_err().code().code, "grants.store");
    assert!(grants.allow_command("x").is_err());
}

/// Spec scenario "Revocation mid-session" at the store level (the CLI test
/// revokes through `prometheus grants revoke`).
#[test]
fn a_revocation_reaches_the_next_check_of_a_running_session() {
    let dir = tempfile::tempdir().unwrap();
    let grants = file(&dir);
    let added = grants
        .add(&plugin(), &caps(&["net.connect:api.example.com"]), "cli")
        .unwrap();
    let session = SessionGrants::new(CapabilitySet::new(), grants.clone());
    let call = Capability::parse("net.connect:api.example.com:443").unwrap();
    session.require(&plugin(), &call).unwrap();

    grants.revoke(added[0].id).unwrap();
    let err = session.require(&plugin(), &call).unwrap_err();
    assert_eq!(
        err.message(),
        "capability not granted: net.connect:api.example.com:443"
    );
    assert_eq!(err.exit_code(), 77);
}

#[test]
fn session_grants_live_only_as_long_as_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let base = caps(&["llm.call"]);
    let session = SessionGrants::new(base.clone(), file(&dir));
    let extra = caps(&["net.connect:intranet.example"]);
    session.grant_for_session(&Subject::Profile, &extra);
    let held = session.capabilities(&Subject::Profile).unwrap();
    assert_eq!(
        held.to_strings(),
        ["net.connect:intranet.example", "llm.call"]
    );
    assert!(session.capabilities(&plugin()).unwrap().is_empty());
    drop(session);

    let next = SessionGrants::new(base, file(&dir));
    assert_eq!(
        next.capabilities(&Subject::Profile).unwrap().to_strings(),
        ["llm.call"]
    );
    assert!(
        !dir.path().join("grants.toml").exists(),
        "nothing persisted"
    );
}

#[test]
fn the_baseline_is_the_configured_list_or_the_built_in_one() {
    let mut security = SecurityConfig::default();
    assert!(baseline(&security).allows(&Capability::parse("process.spawn:git").unwrap()));
    security.capabilities = Some(vec![CapabilityText("llm.call".into())]);
    assert_eq!(baseline(&security).to_strings(), ["llm.call"]);
}
