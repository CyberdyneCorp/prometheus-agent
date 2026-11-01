//! The `todo` list stored with its session (spec: agent-turn-loop, todo
//! tool contract: persisted with the session).

use pmt_core::id::SessionId;
use pmt_store::SessionTodos;
use serde_json::json;

use crate::support::{TestHome, session};

#[test]
fn the_list_survives_reopening_and_goes_with_the_session() {
    let home = TestHome::new();
    let s = {
        let store = home.store("default");
        let s = session(&store);
        assert_eq!(store.todos(s.id).unwrap(), None);
        let todos = SessionTodos {
            revision: 3,
            items: json!([{"id": "a", "content": "write tests", "status": "pending"}]),
        };
        store.save_todos(s.id, &todos).unwrap();
        s
    };
    let store = home.store("default");
    let back = store.todos(s.id).unwrap().unwrap();
    assert_eq!(back.revision, 3);
    assert_eq!(back.items[0]["content"], "write tests");
    store.delete_session(s.id, false).unwrap();
    assert_eq!(store.todos(s.id).unwrap(), None);
}

#[test]
fn saving_for_an_unknown_session_is_not_found() {
    let home = TestHome::new();
    let store = home.store("default");
    let todos = SessionTodos {
        revision: 1,
        items: json!([]),
    };
    let err = store.save_todos(SessionId::new(), &todos).unwrap_err();
    assert_eq!(err.code().code, "session.not_found");
}
