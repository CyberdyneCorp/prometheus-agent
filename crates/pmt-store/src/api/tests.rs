use jiff::Timestamp;

use super::admissions::{Endpoint, Key, Reserved};
use super::runs::{ApiRun, RunUpdate};
use super::{ApiSession, StoredResponse};
use crate::{NewSession, Source, Store};

fn store() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let home = pmt_config::Home::open(&root.path().join("home")).unwrap();
    let dir = home
        .profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    let store = Store::open(&scope).unwrap();
    (root, store)
}

const KEY: Key<'static> = Key {
    endpoint: Endpoint::Chat,
    key_fp: "fp",
    idem_key: "k1",
};

#[test]
fn a_key_is_reserved_once_and_keeps_its_answer() {
    let (_dir, store) = store();
    let Reserved::Fresh(first) = store.reserve_admission(KEY, "id-a", None).unwrap() else {
        panic!("fresh");
    };
    let Reserved::Existing(again) = store.reserve_admission(KEY, "id-b", None).unwrap() else {
        panic!("existing");
    };
    assert_eq!(again.id, first.id);
    assert_eq!(again.identity, "id-a");
    assert_eq!(again.answer, None);
    store
        .settle_admission(&first.id, Some("s1"), "{\"ok\":1}")
        .unwrap();
    let Reserved::Existing(done) = store.reserve_admission(KEY, "id-a", None).unwrap() else {
        panic!("existing");
    };
    assert_eq!(done.answer.as_deref(), Some("{\"ok\":1}"));
    assert_eq!(done.session_id.as_deref(), Some("s1"));
    // Another endpoint or bearer key is another namespace.
    let other = Key {
        key_fp: "fp2",
        ..KEY
    };
    assert!(matches!(
        store.reserve_admission(other, "id-a", None).unwrap(),
        Reserved::Fresh(_)
    ));
    store.release_admission(&first.id).unwrap();
    assert!(matches!(
        store.reserve_admission(KEY, "id-a", None).unwrap(),
        Reserved::Fresh(_)
    ));
}

#[test]
fn deleting_a_session_tombstones_its_admissions_and_forgets_its_rows() {
    let (_dir, store) = store();
    let session = store.create_session(&NewSession::new(Source::Api)).unwrap();
    let id = session.id.to_string();
    store
        .put_api_session(&ApiSession {
            session_id: session.id,
            instructions: Some("be brief".into()),
            session_key: Some("webui:42".into()),
            derived_key: Some("hash".into()),
        })
        .unwrap();
    assert_eq!(
        store.api_session_by_derived("hash").unwrap(),
        Some(session.id)
    );
    assert_eq!(
        store.api_session_by_key("webui:42").unwrap(),
        Some(session.id)
    );
    let Reserved::Fresh(admission) = store.reserve_admission(KEY, "x", Some(&id)).unwrap() else {
        panic!("fresh");
    };
    store.settle_admission(&admission.id, None, "{}").unwrap();
    store.delete_session(session.id, false).unwrap();
    assert_eq!(store.api_session_by_derived("hash").unwrap(), None);
    let Reserved::Existing(blocked) = store.reserve_admission(KEY, "x", None).unwrap() else {
        panic!("existing");
    };
    assert_eq!(blocked.answer, None);
    assert!(blocked.tombstone_until.unwrap() > Timestamp::now());
}

#[test]
fn the_response_store_evicts_the_least_recently_used() {
    let (_dir, store) = store();
    for n in 0..3 {
        let response = StoredResponse {
            id: format!("resp_{n}"),
            session_id: "s".into(),
            conversation: Some("c".into()),
            body: "{}".into(),
            created_at: Timestamp::from_second(1_000 + n).unwrap(),
        };
        store.put_api_response(&response, 2).unwrap();
        if n == 1 {
            // Reading resp_0 would make it recent; read resp_1 instead.
            assert!(store.api_response("resp_1").unwrap().is_some());
        }
    }
    assert!(store.api_response("resp_0").unwrap().is_none());
    assert!(store.api_response("resp_2").unwrap().is_some());
    assert_eq!(
        store.api_conversation_latest("c").unwrap().unwrap().id,
        "resp_2"
    );
    assert!(store.delete_api_response("resp_2").unwrap());
    assert!(!store.delete_api_response("resp_2").unwrap());
}

#[test]
fn a_terminal_run_never_changes_again() {
    let (_dir, store) = store();
    let now = Timestamp::now();
    store
        .insert_api_run(&ApiRun {
            id: "run_1".into(),
            session_id: "s".into(),
            status: "running".into(),
            admission_id: "adm".into(),
            execution_generation: 1,
            owner: "i1".into(),
            requested: None,
            output: None,
            usage: None,
            runtime: None,
            error: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    assert_eq!(store.active_api_runs().unwrap().len(), 1);
    let stopped = store
        .update_api_run(
            "run_1",
            &RunUpdate {
                status: Some("interrupted".into()),
                error: Some("Gateway shutdown interrupted the run.".into()),
                ..RunUpdate::default()
            },
        )
        .unwrap();
    assert_eq!(stopped.status, "interrupted");
    let late = store
        .update_api_run(
            "run_1",
            &RunUpdate {
                status: Some("completed".into()),
                output: Some("late".into()),
                ..RunUpdate::default()
            },
        )
        .unwrap();
    assert_eq!(late.status, "interrupted");
    assert_eq!(late.output, None);
    assert!(store.active_api_runs().unwrap().is_empty());
}
