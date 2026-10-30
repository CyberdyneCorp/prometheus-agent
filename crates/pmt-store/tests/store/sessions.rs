//! Session records, lineage and transactional messages (spec:
//! session-storage, Single session identity format; Session record and
//! source enum; First end reason wins; Typed session lineage;
//! Transactional message persistence).

use pmt_core::id::{MessageId, SessionId};
use pmt_store::{
    Archived, EndReason, LineageKind, NewMessage, NewSession, Origin, Role, Source, Visibility,
};
use serde_json::json;

use crate::support::{TestHome, raw, session};

#[test]
fn cron_session_has_a_plain_ulid_id_and_typed_origin() {
    let home = TestHome::new();
    let store = home.store("default");
    let mut new = NewSession::new(Source::Cron);
    new.origin = Origin::Cron {
        job_id: "daily-digest".to_owned(),
    };
    let created = store.create_session(&new).unwrap();

    let id = created.id.to_string();
    assert_eq!(id.len(), 30);
    assert!(id.starts_with("ses_"));
    assert!(
        id[4..]
            .chars()
            .all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c))),
        "{id}"
    );
    let stored = store.session(created.id).unwrap();
    assert_eq!(stored.source, Source::Cron);
    assert_eq!(stored.origin.job_id(), Some("daily-digest"));
    assert_eq!(stored.profile_id, store.profile_id().to_string());
    assert_eq!(stored.root_session_id, stored.id);
}

#[test]
fn unknown_source_is_rejected_by_the_store_too() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let err = "telegrm".parse::<Source>().unwrap_err();
    assert_eq!(err.code().code, "store.invalid_enum");
    assert!(err.message().contains("source"), "{err}");
    // A row written behind the API's back cannot hold an unknown value.
    let refused = raw(&home.database("default")).execute(
        "UPDATE sessions SET source = 'telegrm' WHERE id = ?1",
        [chat.id.to_string()],
    );
    assert!(refused.is_err());
}

#[test]
fn first_end_reason_wins_and_resume_clears_it() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    assert!(store.end_session(chat.id, EndReason::Compression).unwrap());
    assert!(!store.end_session(chat.id, EndReason::UserExit).unwrap());
    let ended = store.session(chat.id).unwrap();
    assert_eq!(ended.end_reason, Some(EndReason::Compression));
    assert!(ended.ended_at.is_some());

    let reopened = store.reopen_session(chat.id).unwrap();
    assert_eq!(reopened.end_reason, None);
    assert_eq!(reopened.ended_at, None);
    assert!(store.end_session(chat.id, EndReason::UserExit).unwrap());
}

#[test]
fn resume_clears_an_auto_archive_but_keeps_a_user_archive() {
    let home = TestHome::new();
    let store = home.store("default");
    let (auto, user) = (session(&store), session(&store));
    let db = raw(&home.database("default"));
    for (chat, flag) in [(&auto, "auto"), (&user, "user")] {
        db.execute(
            "UPDATE sessions SET archived = ?2 WHERE id = ?1",
            [chat.id.to_string(), flag.to_owned()],
        )
        .unwrap();
    }
    assert_eq!(
        store.reopen_session(auto.id).unwrap().archived,
        Archived::None
    );
    assert_eq!(
        store.reopen_session(user.id).unwrap().archived,
        Archived::User
    );
}

fn child(store: &pmt_store::Store, parent: SessionId, kind: LineageKind) -> SessionId {
    store
        .create_session(&NewSession::child(Source::Cli, parent, kind))
        .unwrap()
        .id
}

#[test]
fn compression_chain_lineage_and_tip() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store).id;
    let b = child(&store, a, LineageKind::Compression);
    let c = child(&store, b, LineageKind::Compression);

    assert_eq!(store.lineage(a).unwrap(), [a, b, c]);
    assert_eq!(store.compression_tip(a).unwrap(), c);
    assert_eq!(store.lineage(c).unwrap(), [a, b, c]);
    assert_eq!(store.session(c).unwrap().root_session_id, a);
}

#[test]
fn fork_is_not_mistaken_for_a_continuation() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store).id;
    store.end_session(a, EndReason::Compression).unwrap();
    let fork = child(&store, a, LineageKind::Fork);

    assert_eq!(store.compression_tip(a).unwrap(), a);
    assert_eq!(store.lineage(fork).unwrap(), [a, fork]);
    let tree = store.tree(a).unwrap();
    assert_eq!(tree.len(), 1);
    assert_eq!((tree[0].id, tree[0].kind), (fork, LineageKind::Fork));
}

#[test]
fn tree_lists_every_descendant_with_its_kind() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store).id;
    let b = child(&store, a, LineageKind::Compression);
    let f = child(&store, a, LineageKind::Fork);
    let d = child(&store, b, LineageKind::Delegate);
    let tree = store.tree(a).unwrap();
    let nodes: Vec<_> = tree
        .iter()
        .map(|n| (n.id, n.parent, n.kind, n.depth))
        .collect();
    assert_eq!(
        nodes,
        [
            (b, a, LineageKind::Compression, 1),
            (f, a, LineageKind::Fork, 1),
            (d, b, LineageKind::Delegate, 2),
        ]
    );
    assert_eq!(store.session(d).unwrap().root_session_id, a);
}

#[test]
fn child_of_unknown_parent_is_refused() {
    let home = TestHome::new();
    let store = home.store("default");
    let err = store
        .create_session(&NewSession::child(
            Source::Cli,
            SessionId::new(),
            LineageKind::Fork,
        ))
        .unwrap_err();
    assert_eq!(err.code().code, "session.not_found");
}

#[test]
fn messages_get_increasing_seqs_and_counters_in_one_write() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let mut call = NewMessage::new(Role::Assistant, "");
    call.tool_calls = Some(json!([{"id": "c1", "name": "read"}, {"id": "c2", "name": "grep"}]));
    let seqs = store
        .append_messages(chat.id, &[NewMessage::new(Role::User, "find it"), call])
        .unwrap();
    let more = store
        .append_message(chat.id, &NewMessage::new(Role::Tool, "found"))
        .unwrap();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(more, 3);
    let after = store.session(chat.id).unwrap();
    assert_eq!(after.message_count, 3);
    assert_eq!(after.tool_call_count, 2);
    assert!(after.last_activity_at >= chat.last_activity_at);
    let stored = store.messages(chat.id, Visibility::Active).unwrap();
    assert_eq!(stored[1].tool_calls.as_ref().unwrap()[1]["name"], "grep");
}

#[test]
fn batch_failure_leaves_no_partial_rows() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let existing = NewMessage::new(Role::User, "first");
    store.append_message(chat.id, &existing).unwrap();
    let before = store.session(chat.id).unwrap();
    let mut batch: Vec<NewMessage> = (0..5)
        .map(|i| NewMessage::new(Role::Assistant, format!("part {i}")))
        .collect();
    batch[3].id = existing.id;

    let err = store.append_messages(chat.id, &batch).unwrap_err();

    assert_eq!(err.code().code, "store.constraint");
    assert_eq!(store.messages(chat.id, Visibility::All).unwrap().len(), 1);
    let after = store.session(chat.id).unwrap();
    assert_eq!(after.message_count, before.message_count);
    assert_eq!(after.last_activity_at, before.last_activity_at);
}

#[test]
fn large_tool_output_is_spilled_to_a_blob() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let output = "x".repeat(200 * 1024);
    let mut result = NewMessage::new(Role::Tool, output.clone());
    result.tool_call_id = Some("c1".to_owned());
    store.append_message(chat.id, &result).unwrap();

    let reference: String = raw(&home.database("default"))
        .query_row(
            "SELECT content_ref FROM messages WHERE id = ?1 AND content IS NULL",
            [result.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    let hex = reference.strip_prefix("blob:sha256:").unwrap();
    assert_eq!(hex.len(), 64);
    let file = store.location().blobs().join(&hex[..2]).join(hex);
    assert_eq!(std::fs::read(file).unwrap(), output.as_bytes());
    let read = &store.messages(chat.id, Visibility::Active).unwrap()[0];
    assert_eq!(read.content.as_deref(), Some(output.as_str()));
    assert_eq!(read.content_ref.as_ref().unwrap().to_string(), reference);
}

#[test]
fn message_ids_are_durable_and_kept_when_given() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let mut imported = NewMessage::new(Role::User, "from an export");
    imported.id = MessageId::parse("msg_01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    store.append_message(chat.id, &imported).unwrap();
    assert_eq!(
        store.messages(chat.id, Visibility::All).unwrap()[0].id,
        imported.id
    );
}
