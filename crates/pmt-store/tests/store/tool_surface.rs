//! The frozen tool surface a session records (spec: tool-system, Tool
//! introspection commands).

use jiff::Timestamp;
use pmt_core::id::SessionId;
use pmt_store::tool_surface::{RecordedSurface, SurfaceTool};

use crate::support::{TestHome, session};

fn surface(generation: u64, exposure: &str) -> RecordedSurface {
    RecordedSurface {
        generation,
        recorded_at: Timestamp::from_millisecond(1_700_000_000_000).unwrap(),
        tools: vec![SurfaceTool {
            name: "todo".into(),
            toolset: "todo".into(),
            owner: "builtin".into(),
            exposure: exposure.into(),
        }],
    }
}

#[test]
fn a_rebuild_replaces_the_record_and_deletion_removes_it() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    assert_eq!(store.tool_surface(s.id).unwrap(), None);

    store
        .record_tool_surface(s.id, &surface(1, "exposed"))
        .unwrap();
    store
        .record_tool_surface(s.id, &surface(2, "not_in_toolset"))
        .unwrap();
    assert_eq!(
        store.tool_surface(s.id).unwrap(),
        Some(surface(2, "not_in_toolset"))
    );

    store.delete_session(s.id, false).unwrap();
    assert_eq!(store.tool_surface(s.id).unwrap(), None);
}

#[test]
fn recording_for_an_unknown_session_is_not_found() {
    let home = TestHome::new();
    let store = home.store("default");
    let err = store
        .record_tool_surface(SessionId::new(), &surface(1, "exposed"))
        .unwrap_err();
    assert_eq!(err.code().code, "session.not_found");
}
