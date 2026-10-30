//! Titles and session references (spec: session-storage, Session titles
//! with provenance; Title lineage numbering and resolution).

use pmt_core::id::SessionId;
use pmt_store::{LineageKind, NewSession, Source, Store, TitleSource, TitleWrite};

use crate::support::{TestHome, session};

fn titled(store: &Store, title: &str) -> SessionId {
    let mut new = NewSession::new(Source::Cli);
    new.title = Some(title.to_owned());
    store.create_session(&new).unwrap().id
}

fn title(store: &Store, id: SessionId) -> Option<String> {
    store.session(id).unwrap().title
}

#[test]
fn auto_title_does_not_overwrite_a_user_title() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    store
        .set_title(chat, Some("Tax prep"), TitleSource::User)
        .unwrap();

    let outcome = store
        .set_title(chat, Some("Spreadsheet help"), TitleSource::Llm)
        .unwrap();

    assert_eq!(outcome, TitleWrite::Kept);
    assert_eq!(title(&store, chat).as_deref(), Some("Tax prep"));
    assert_eq!(
        store.session(chat).unwrap().title_source,
        Some(TitleSource::User)
    );
}

#[test]
fn provenance_only_ever_rises_for_automatic_titles() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let write = |t: &str, s| store.set_title(chat, Some(t), s).unwrap();
    assert_eq!(
        write("first words", TitleSource::Derived),
        TitleWrite::Written
    );
    assert_eq!(write("other words", TitleSource::Derived), TitleWrite::Kept);
    assert_eq!(write("Model title", TitleSource::Llm), TitleWrite::Written);
    assert_eq!(
        write("Second model title", TitleSource::Llm),
        TitleWrite::Kept
    );
    assert_eq!(write("Mine", TitleSource::User), TitleWrite::Written);
    assert_eq!(write("Mine again", TitleSource::User), TitleWrite::Written);
    assert_eq!(title(&store, chat).as_deref(), Some("Mine again"));
}

#[test]
fn duplicate_title_names_the_holder() {
    let home = TestHome::new();
    let store = home.store("default");
    let holder = titled(&store, "Refactor");
    let other = session(&store).id;

    let err = store
        .set_title(other, Some("Refactor"), TitleSource::User)
        .unwrap_err();

    assert_eq!(
        err.message(),
        format!("Title 'Refactor' is already in use by session {holder}")
    );
    assert_eq!(title(&store, other), None);
}

#[test]
fn titles_are_sanitised_and_length_checked() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    store
        .set_title(
            chat,
            Some("  Fix\u{200B}  the\u{202E}\tparser "),
            TitleSource::User,
        )
        .unwrap();
    assert_eq!(title(&store, chat).as_deref(), Some("Fix the parser"));
    let err = store
        .set_title(chat, Some(&"t".repeat(120)), TitleSource::User)
        .unwrap_err();
    assert_eq!(err.message(), "Title too long (120 chars, max 100)");
    store
        .set_title(chat, Some(" \u{FEFF} "), TitleSource::User)
        .unwrap();
    assert_eq!(title(&store, chat), None, "empty means untitled");
}

#[test]
fn continuations_and_forks_are_numbered() {
    let home = TestHome::new();
    let store = home.store("default");
    let original = titled(&store, "Refactor");
    let child = |parent, kind| {
        store
            .create_session(&NewSession::child(Source::Cli, parent, kind))
            .unwrap()
            .id
    };
    let continued = child(original, LineageKind::Compression);
    let fork = child(original, LineageKind::Fork);
    let from_fork = child(fork, LineageKind::Compression);
    let delegate = child(original, LineageKind::Delegate);

    assert_eq!(title(&store, continued).as_deref(), Some("Refactor #2"));
    assert_eq!(title(&store, fork).as_deref(), Some("Refactor #3"));
    assert_eq!(title(&store, from_fork).as_deref(), Some("Refactor #4"));
    assert_eq!(title(&store, delegate), None);
    assert_eq!(
        store.session(fork).unwrap().title_source,
        Some(TitleSource::User)
    );
}

#[test]
fn resume_by_base_title_picks_the_newest_number() {
    let home = TestHome::new();
    let store = home.store("default");
    let first = titled(&store, "Refactor");
    let second = store
        .create_session(&NewSession::child(
            Source::Cli,
            first,
            LineageKind::Compression,
        ))
        .unwrap()
        .id;
    titled(&store, "Refactoring notes");

    assert_eq!(store.resolve_session("Refactor").unwrap(), second);
    assert_eq!(store.resolve_session("Refactor #2").unwrap(), second);
    assert_eq!(
        store
            .resolve_session("Refactor #7")
            .unwrap_err()
            .code()
            .code,
        "session.not_found"
    );
}

#[test]
fn exact_id_and_unambiguous_prefix_resolve() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let text = chat.to_string();
    assert_eq!(store.resolve_session(&text).unwrap(), chat);
    // The only session, so any prefix of 8+ characters is unambiguous.
    assert_eq!(store.resolve_session(&text[..20]).unwrap(), chat);
    assert_eq!(
        store.resolve_session(&text[4..20].to_lowercase()).unwrap(),
        chat
    );
}

#[test]
fn ambiguous_and_short_prefixes_are_refused() {
    let home = TestHome::new();
    let store = home.store("default");
    let ids: Vec<String> = (0..3).map(|_| session(&store).id.to_string()).collect();
    // Ids made in one process share their timestamp prefix.
    let shared = &ids[0][..10];
    assert!(ids.iter().all(|id| id.starts_with(shared)));

    let err = store.resolve_session(shared).unwrap_err();
    assert_eq!(err.code().code, "session.ambiguous");
    assert!(
        ids.iter().all(|id| err.message().contains(id.as_str())),
        "{err}"
    );
    let err = store.resolve_session(&ids[0][4..11]).unwrap_err();
    assert_eq!(
        err.code().code,
        "session.not_found",
        "7 characters are too few"
    );
}
