//! Session forking (spec: session-storage, Session forking API).

use pmt_core::id::SessionId;
use pmt_store::{LineageKind, NewMessage, Role, Source, Store, TitleSource, Visibility};

use crate::support::TestHome;

fn conversation(store: &Store, turns: usize) -> SessionId {
    let session = store
        .create_session(&pmt_store::NewSession::new(Source::Cli))
        .unwrap()
        .id;
    store
        .set_title(session, Some("Tax prep"), TitleSource::User)
        .unwrap();
    for n in 0..turns {
        store
            .append_messages(
                session,
                &[
                    NewMessage::new(Role::User, format!("question {n}")),
                    NewMessage::new(Role::Assistant, format!("answer {n}")),
                ],
            )
            .unwrap();
    }
    session
}

#[test]
fn a_fork_copies_the_active_messages_up_to_the_chosen_one() {
    let home = TestHome::new();
    let store = home.store("default");
    let parent = conversation(&store, 5);
    let messages = store.messages(parent, Visibility::Active).unwrap();
    assert_eq!(messages.len(), 10);
    let fork = store
        .fork_session(parent, Some(messages[3].id), None)
        .unwrap();
    assert_eq!(fork.lineage_kind, Some(LineageKind::Fork));
    assert_eq!(fork.parent_session_id, Some(parent));
    assert_eq!(fork.title.as_deref(), Some("Tax prep #2"));
    let copied = store.messages(fork.id, Visibility::Active).unwrap();
    let origins: Vec<String> = copied
        .iter()
        .map(|m| {
            m.metadata.as_ref().unwrap()["origin_message_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let expected: Vec<String> = messages[..4].iter().map(|m| m.id.to_string()).collect();
    assert_eq!(origins, expected);
    assert!(
        copied
            .iter()
            .zip(&messages)
            .all(|(c, m)| c.id != m.id && c.content == m.content)
    );
    assert_eq!(
        store.messages(parent, Visibility::Active).unwrap().len(),
        10
    );
}

#[test]
fn a_fork_defaults_to_the_last_message_and_refuses_a_foreign_one() {
    let home = TestHome::new();
    let store = home.store("default");
    let parent = conversation(&store, 2);
    let other = conversation_untitled(&store);
    let fork = store.fork_session(parent, None, Some("Branch")).unwrap();
    assert_eq!(fork.title.as_deref(), Some("Branch"));
    assert_eq!(
        store.messages(fork.id, Visibility::Active).unwrap().len(),
        4
    );
    let foreign = store.messages(other, Visibility::Active).unwrap()[0].id;
    let err = store.fork_session(parent, Some(foreign), None).unwrap_err();
    assert_eq!(err.code().code, "store.fork_target_invalid");
}

fn conversation_untitled(store: &Store) -> SessionId {
    let session = store
        .create_session(&pmt_store::NewSession::new(Source::Cli))
        .unwrap()
        .id;
    store
        .append_message(session, &NewMessage::new(Role::User, "elsewhere"))
        .unwrap();
    session
}
