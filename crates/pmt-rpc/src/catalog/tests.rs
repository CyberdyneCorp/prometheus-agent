use serde_json::json;

use super::contracts::{Method, Ping};
use super::{Catalog, INDEX, IndexEntry, Kind};

/// A method no capability indexed.
struct Frobnicate;

impl Method for Frobnicate {
    const NAME: &'static str = "session.frobnicate";
    type Params = super::contracts::Empty;
    type Result = super::contracts::Ack;
}

#[test]
fn the_schema_records_each_name_with_its_owning_capability() {
    let schema = Catalog::builtin().schema();
    assert_eq!(schema["methods"]["worktree.new"]["owner"], "git-worktrees");
    assert_eq!(schema["methods"]["journey.list"]["owner"], "memory");
    assert_eq!(
        schema["events"]["automation.state"]["owner"],
        "session-automation"
    );
    assert_eq!(
        schema["server_requests"]["approval.request"]["owner"],
        "workspace-architecture"
    );
    for entry in INDEX {
        let section = &schema[entry.kind.section()];
        assert!(section.get(entry.name).is_some(), "{} missing", entry.name);
    }
    assert!(schema["methods"]["prompt.submit"]["params"].is_object());
    assert_eq!(schema["methods"]["evolve.start"]["declared"], false);
}

#[test]
fn a_contract_for_an_unregistered_name_fails_at_startup() {
    let err = Catalog::build(Catalog::builtin().protocol(), INDEX, |d| {
        d.method::<Ping>().method::<Frobnicate>();
    })
    .unwrap_err();
    assert!(
        err.contains("session.frobnicate is not in the catalog index"),
        "{err}"
    );
    let mut twice = INDEX.to_vec();
    twice.push(IndexEntry {
        name: "rpc.ping",
        kind: Kind::Method,
        owner: "x",
        since: 0,
        long: false,
    });
    assert!(Catalog::build(Catalog::builtin().protocol(), &twice, |_| {}).is_err());
}

#[test]
fn a_misspelled_param_names_its_path() {
    let err = Catalog::builtin()
        .check_params(
            "session.resume",
            &json!({"sesion_id": "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W"}),
        )
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    let data = err.data.unwrap();
    assert_eq!(data.path.as_deref(), Some("/sesion_id"));
    assert_eq!(data.code, "rpc.unknown_field");
    assert_eq!(data.kind, "invalid_input");
}

#[test]
fn a_missing_param_names_the_field() {
    let err = Catalog::builtin()
        .check_params("prompt.submit", &json!({"text": "hi"}))
        .unwrap_err();
    assert_eq!(err.code, -32_602);
    assert_eq!(err.data.unwrap().path.as_deref(), Some("/session_id"));
}

#[test]
fn the_digest_is_stable() {
    assert_eq!(Catalog::builtin().digest(), Catalog::builtin().digest());
    assert_eq!(Catalog::builtin().digest().len(), 64);
}
