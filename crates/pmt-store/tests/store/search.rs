//! Full-text search, query sanitisation and fail-open indexes (spec:
//! session-storage, Full-text session search; Search query sanitisation;
//! Derived indexes fail open and rebuild offline).

use pmt_core::doctor::Status;
use pmt_store::{
    NewMessage, NewSession, Role, SearchMode, SearchOptions, SearchSort, Source, Store,
};

use crate::support::{TestHome, raw, session};

fn say(
    store: &Store,
    session: pmt_core::id::SessionId,
    role: Role,
    text: &str,
) -> pmt_core::id::MessageId {
    let message = NewMessage::new(role, text);
    store.append_message(session, &message).unwrap();
    message.id
}

fn search(store: &Store, query: &str) -> pmt_store::SearchResults {
    store.search(query, &SearchOptions::default()).unwrap()
}

#[test]
fn word_search_ranks_and_returns_hit_fields() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let hit = say(
        &store,
        chat.id,
        Role::User,
        "How do I scale the kubernetes deployment?",
    );
    say(
        &store,
        chat.id,
        Role::Assistant,
        "Use a HorizontalPodAutoscaler.",
    );

    let results = search(&store, "kubernetes");

    assert_eq!(results.mode, SearchMode::Word);
    assert_eq!(results.hits.len(), 1);
    let first = &results.hits[0];
    assert_eq!(
        (first.session_id, first.message_id, first.role),
        (chat.id, hit, Role::User)
    );
    assert!(first.snippet.contains("«kubernetes»"), "{}", first.snippet);
    assert!(first.snippet.chars().count() <= 200);
    assert!(first.score.is_some(), "BM25 ranked");
}

#[test]
fn rewound_message_is_hidden_unless_inactive_rows_are_asked_for() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(&store, chat.id, Role::User, "first question");
    let target = say(&store, chat.id, Role::User, "deploy to kubernetes please");
    store.rewind(chat.id, target, None, None).unwrap();

    assert!(search(&store, "kubernetes").hits.is_empty());
    let with_inactive = SearchOptions {
        include_inactive: true,
        ..SearchOptions::default()
    };
    assert_eq!(
        store
            .search("kubernetes", &with_inactive)
            .unwrap()
            .hits
            .len(),
        1
    );
}

#[test]
fn cjk_query_is_answered_from_the_trigram_index() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let hit = say(
        &store,
        chat.id,
        Role::User,
        "この設定ファイルを読んでください",
    );
    say(&store, chat.id, Role::User, "unrelated english text");

    let results = search(&store, "設定ファイル");

    assert_eq!(results.mode, SearchMode::Trigram);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].message_id, hit);
}

#[test]
fn short_terms_use_the_trigram_index() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(&store, chat.id, Role::User, "run go vet on the module");
    let results = search(&store, "go");
    assert_eq!(results.mode, SearchMode::Trigram);
    assert_eq!(results.hits.len(), 1);
}

#[test]
fn hostile_fts_syntax_never_fails() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(
        &store,
        chat.id,
        Role::User,
        "call chat-send with NEAR(a b) and config.toml",
    );
    for hostile in [
        "chat-send",
        "\"unbalanced",
        "NOT",
        "AND OR NOT",
        "*",
        "col:value",
        "NEAR(a b",
        "a OR",
        "^start {x} [y] (z",
        "\"\"",
        "'; DROP TABLE messages; --",
        &"x".repeat(5000),
    ] {
        let results = store.search(hostile, &SearchOptions::default());
        assert!(results.is_ok(), "{hostile:?}: {results:?}");
    }
    let hyphen = search(&store, "chat-send");
    assert_eq!(hyphen.hits.len(), 1, "searched as the phrase \"chat-send\"");
    assert_eq!(search(&store, "config.toml").hits.len(), 1);
}

#[test]
fn filters_and_paging_apply() {
    let home = TestHome::new();
    let store = home.store("default");
    let cli = session(&store);
    let cron = store
        .create_session(&NewSession::new(Source::Cron))
        .unwrap();
    for n in 0..5 {
        say(&store, cli.id, Role::User, &format!("deploy step {n}"));
    }
    say(&store, cron.id, Role::Assistant, "deploy report");

    let only_cron = SearchOptions {
        sources: vec![Source::Cron],
        ..SearchOptions::default()
    };
    assert_eq!(store.search("deploy", &only_cron).unwrap().hits.len(), 1);
    let users = SearchOptions {
        roles: vec![Role::User],
        sort: SearchSort::Oldest,
        limit: 2,
        offset: 1,
        ..SearchOptions::default()
    };
    let page = store.search("deploy", &users).unwrap();
    assert_eq!(page.hits.len(), 2);
    assert!(page.hits[0].snippet.contains("step 1"), "{:?}", page.hits);
    let in_session = SearchOptions {
        session_id: Some(cron.id),
        ..SearchOptions::default()
    };
    assert_eq!(
        store.search("deploy", &in_session).unwrap().hits[0].session_id,
        cron.id
    );
}

#[test]
fn compacted_rows_stay_searchable() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(
        &store,
        chat.id,
        Role::User,
        "the original postgres question",
    );
    let read = store
        .messages(chat.id, pmt_store::Visibility::Active)
        .unwrap();
    let lock = match store
        .acquire_lease(
            chat.id,
            pmt_store::LeaseKind::Compaction,
            &pmt_store::Holder::current(),
        )
        .unwrap()
    {
        pmt_store::Acquire::Granted(lease) => lease,
        busy @ pmt_store::Acquire::Busy { .. } => panic!("{busy:?}"),
    };
    let compaction = pmt_store::Compaction::new(
        &read,
        vec![read[0].id],
        vec![NewMessage::new(Role::User, "summary")],
    );
    store.compact(&lock, &compaction).unwrap();
    assert_eq!(search(&store, "postgres").hits.len(), 1);
}

#[test]
fn dropped_index_fails_open_and_doctor_fix_rebuilds_it() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(&store, chat.id, Role::User, "before the damage: kubernetes");
    raw(&home.database("default"))
        .execute_batch("DROP TABLE messages_fts;")
        .unwrap();

    // The canonical write still commits; the index is marked stale.
    let late = say(
        &store,
        chat.id,
        Role::User,
        "after the damage: kubernetes again",
    );
    assert!(store.indexes_stale().unwrap());
    let scanned = search(&store, "kubernetes");
    assert_eq!(scanned.mode, SearchMode::Scan);
    assert_eq!(scanned.hits.len(), 2);
    assert!(scanned.hits.iter().any(|h| h.message_id == late));
    let checks = store.doctor_checks().unwrap();
    assert_eq!(checks[0].status, Status::Warn, "{checks:?}");

    let fixed = store.doctor_fix().unwrap();

    assert_eq!(fixed.len(), 1, "{fixed:?}");
    assert!(!store.indexes_stale().unwrap());
    let indexed = search(&store, "kubernetes");
    assert_eq!(indexed.mode, SearchMode::Word);
    assert_eq!(indexed.hits.len(), 2);
    assert_eq!(store.doctor_checks().unwrap()[0].status, Status::Ok);
    assert!(
        store.doctor_fix().unwrap().is_empty(),
        "nothing left to fix"
    );
}

#[test]
fn damaged_index_without_writes_is_detected_and_searches_still_work() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(&store, chat.id, Role::User, "kubernetes");
    raw(&home.database("default"))
        .execute_batch("DROP TABLE messages_trigram;")
        .unwrap();
    assert_eq!(store.doctor_checks().unwrap()[0].status, Status::Warn);
    // A trigram query fails on the missing table and falls back to a scan.
    let results = search(&store, "ku");
    assert_eq!(results.mode, SearchMode::Scan);
    assert_eq!(results.hits.len(), 1);
}

#[test]
fn spilled_messages_are_found_after_a_rebuild() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let big = format!("needle-in-a-haystack {}", "hay ".repeat(20_000));
    say(&store, chat.id, Role::Tool, &big);
    assert_eq!(search(&store, "needle-in-a-haystack").hits.len(), 1);
    store.repair_indexes().unwrap();
    assert_eq!(search(&store, "needle-in-a-haystack").hits.len(), 1);
}
