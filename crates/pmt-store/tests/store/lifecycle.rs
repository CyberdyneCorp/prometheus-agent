//! Rewind, deletion, retention, size guards, usage events and the
//! integration source tag (spec: session-storage, Rewind is a soft delete;
//! Deletion semantics; Retention, pruning and auto-archive; Resume and
//! export size guards; Usage events keyed by root lineage; Integration
//! source tag).

use std::time::Duration;

use pmt_config::schema::SessionsConfig;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{
    Acquire, Archived, CostStatus, EndReason, Holder, LeaseKind, LineageKind, ListOptions,
    NewMessage, NewSession, Role, Source, Store, Tokens, UsageEvent, UsageScope, Visibility,
};

use crate::support::{TestHome, raw, session};

fn say(store: &Store, session: SessionId, role: Role, text: &str) -> MessageId {
    let message = NewMessage::new(role, text);
    store.append_message(session, &message).unwrap();
    message.id
}

fn child(store: &Store, parent: SessionId, kind: LineageKind) -> SessionId {
    store
        .create_session(&NewSession::child(Source::Cli, parent, kind))
        .unwrap()
        .id
}

/// Pretends `id` ended and was last active `days` ago.
fn age(home: &TestHome, id: SessionId, days: i64) {
    let ms = jiff::Timestamp::now().as_millisecond() - days * 86_400_000;
    raw(&home.database("default"))
        .execute(
            "UPDATE sessions SET ended_at = ?2, end_reason = 'user_exit', last_activity_at = ?2 \
             WHERE id = ?1",
            rusqlite::params![id.to_string(), ms],
        )
        .unwrap();
}

fn turn_lease(store: &Store, id: SessionId, holder: &Holder) -> Acquire {
    store.acquire_lease(id, LeaseKind::Turn, holder).unwrap()
}

fn other_holder() -> Holder {
    Holder {
        pid: 1,
        process_start: 1,
        host_id: "another-host".to_owned(),
    }
}

#[test]
fn undo_hides_the_last_turn_but_keeps_it_restorable() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    say(&store, chat.id, Role::User, "hello");
    say(&store, chat.id, Role::Assistant, "hi");
    let x = say(&store, chat.id, Role::User, "do the thing");
    let later = [
        say(&store, chat.id, Role::Assistant, "calling a tool"),
        say(&store, chat.id, Role::Tool, "tool output"),
        say(&store, chat.id, Role::Assistant, "done"),
    ];

    let rewound = store.undo_last_turn(chat.id, None).unwrap();

    let mut expected = vec![x];
    expected.extend(later);
    assert_eq!(rewound.messages, expected);
    assert_eq!(rewound.batch, 1);
    let active = store.messages(chat.id, Visibility::Active).unwrap();
    assert_eq!(
        active.len(),
        2,
        "only the first exchange is sent to the model"
    );
    let all = store.messages(chat.id, Visibility::All).unwrap();
    assert_eq!(all.len(), 6, "rewound rows stay in the database");
    assert_eq!(store.session(chat.id).unwrap().rewind_count, 1);

    let restored = store.restore_rewind(chat.id, x, None).unwrap();
    assert_eq!(restored.messages, expected);
    assert_eq!(
        store.messages(chat.id, Visibility::Active).unwrap().len(),
        6
    );
}

#[test]
fn rewind_validates_target_expectations_and_turn_lease() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let user = say(&store, chat.id, Role::User, "question");
    let answer = say(&store, chat.id, Role::Assistant, "answer");

    let not_user = store.rewind(chat.id, answer, None, None).unwrap_err();
    assert_eq!(not_user.code().code, "store.rewind_target_invalid");
    let missing = store
        .rewind(chat.id, MessageId::new(), None, None)
        .unwrap_err();
    assert_eq!(missing.code().code, "store.rewind_target_invalid");
    let stale = store
        .rewind(chat.id, user, Some(&[user]), None)
        .unwrap_err();
    assert_eq!(stale.code().code, "store.rewind_conflict");

    let Acquire::Granted(_) = turn_lease(&store, chat.id, &other_holder()) else {
        panic!("granted")
    };
    let busy = store.rewind(chat.id, user, None, None).unwrap_err();
    assert_eq!(busy.code().code, "session.in_use");
    assert_eq!(
        store.messages(chat.id, Visibility::Active).unwrap().len(),
        2,
        "unchanged"
    );
}

#[test]
fn rewind_by_the_turn_holder_and_restore_after_new_turn_conflicts() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let user = say(&store, chat.id, Role::User, "question");
    let answer = say(&store, chat.id, Role::Assistant, "answer");
    let Acquire::Granted(lease) = turn_lease(&store, chat.id, &Holder::current()) else {
        panic!("granted")
    };
    store
        .rewind(chat.id, user, Some(&[user, answer]), Some(&lease))
        .unwrap();
    say(&store, chat.id, Role::User, "a different question");
    let err = store
        .restore_rewind(chat.id, user, Some(&lease))
        .unwrap_err();
    assert_eq!(err.code().code, "store.rewind_conflict");
}

#[test]
fn deleting_a_forked_parent_keeps_the_fork() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    say(&store, a.id, Role::User, "parent text");
    let fork = child(&store, a.id, LineageKind::Fork);
    let delegate = child(&store, a.id, LineageKind::Delegate);
    let continuation = child(&store, a.id, LineageKind::Compression);

    let deleted = store.delete_session(a.id, false).unwrap();

    assert_eq!(deleted.sessions, vec![a.id, delegate]);
    let f = store.session(fork).unwrap();
    assert!(f.parent_deleted);
    assert_eq!(f.parent_session_id, None);
    assert_eq!(f.lineage_kind, Some(LineageKind::Fork));
    assert!(store.session(continuation).unwrap().parent_deleted);
    assert_eq!(
        store.session(a.id).unwrap_err().code().code,
        "session.not_found"
    );
    assert_eq!(
        store.session(delegate).unwrap_err().code().code,
        "session.not_found"
    );
    let left: i64 = raw(&home.database("default"))
        .query_row(
            "SELECT count(*) FROM messages WHERE session_id = ?1",
            [a.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn delete_with_lineage_removes_continuations_usage_and_blobs() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    let b = child(&store, a.id, LineageKind::Compression);
    let big = "x".repeat(100 * 1024);
    store
        .append_message(b, &NewMessage::new(Role::Tool, big))
        .unwrap();
    store.record_usage(&usage(b, "c1", 10)).unwrap();
    let blobs_before = blob_files(&home);
    assert_eq!(blobs_before, 1);

    let deleted = store.delete_session(a.id, true).unwrap();

    assert_eq!(deleted.sessions, vec![a.id, b]);
    assert_eq!(deleted.blobs, 1);
    assert_eq!(blob_files(&home), 0);
    assert_eq!(store.usage_totals(UsageScope::Profile).unwrap().calls, 0);
}

#[test]
fn delete_is_refused_while_a_lease_is_live() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    let Acquire::Granted(_) = turn_lease(&store, a.id, &other_holder()) else {
        panic!("granted")
    };
    let err = store.delete_session(a.id, false).unwrap_err();
    assert_eq!(err.code().code, "session.in_use");
    assert_eq!(err.exit_code(), 75);
    store.session(a.id).unwrap();
}

#[test]
fn deleting_and_pruning_sessions_removes_their_spillover() {
    let home = TestHome::new();
    let store = home.store("default");
    let (a, b, kept) = (session(&store), session(&store), session(&store));
    let spill = |id: SessionId| store.location().spillover().join(id.to_string());
    for id in [a.id, b.id, kept.id] {
        std::fs::create_dir_all(spill(id)).unwrap();
        std::fs::write(spill(id).join("call_1.txt"), "big").unwrap();
    }
    assert!(store.location().spillover().ends_with("cache/spillover"));

    store.delete_session(a.id, false).unwrap();
    assert!(!spill(a.id).exists());

    age(&home, b.id, 100);
    store.prune(Duration::from_hours(30 * 24), false).unwrap();
    assert!(!spill(b.id).exists());
    assert!(spill(kept.id).join("call_1.txt").exists());
}

fn blob_files(home: &TestHome) -> usize {
    let dir = home.scope("default").path("blobs");
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| std::fs::read_dir(e.path()).unwrap().count())
        .sum()
}

#[test]
fn retention_prunes_only_eligible_sessions() {
    let home = TestHome::new();
    let store = home.store("default");
    let old = session(&store);
    let pinned = session(&store);
    let recent = session(&store);
    let running = session(&store);
    let leased = session(&store);
    age(&home, old.id, 100);
    age(&home, pinned.id, 100);
    store.set_pinned(pinned.id, true).unwrap();
    age(&home, recent.id, 5);
    raw(&home.database("default"))
        .execute(
            "UPDATE sessions SET last_activity_at = 0 WHERE id = ?1",
            [running.id.to_string()],
        )
        .unwrap();
    age(&home, leased.id, 100);
    let Acquire::Granted(_) = turn_lease(&store, leased.id, &other_holder()) else {
        panic!("granted")
    };

    let would = store.prune(Duration::from_hours(30 * 24), true).unwrap();
    assert_eq!(would, vec![old.id], "dry run lists, deletes nothing");
    store.session(old.id).unwrap();

    let config = SessionsConfig {
        retention_secs: 30 * 86_400,
        auto_archive_secs: 0,
        ..SessionsConfig::default()
    };
    let report = store.maintenance(&config).unwrap();

    assert!(report.ran);
    assert_eq!(report.pruned, vec![old.id]);
    for kept in [pinned.id, recent.id, running.id, leased.id] {
        store.session(kept).unwrap();
    }
    assert!(
        !store.maintenance(&config).unwrap().ran,
        "at most once per 24 hours"
    );
}

#[test]
fn compression_chain_is_pruned_only_as_a_whole() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    let b = child(&store, a.id, LineageKind::Compression);
    let c = child(&store, b, LineageKind::Compression);
    for id in [a.id, b] {
        age(&home, id, 100);
    }
    age(&home, c, 1);
    let month = Duration::from_hours(30 * 24);
    assert!(
        store.prune(month, false).unwrap().is_empty(),
        "the tip is recent"
    );
    age(&home, c, 100);
    let mut pruned = store.prune(month, false).unwrap();
    pruned.sort();
    let mut chain = vec![a.id, b, c];
    chain.sort();
    assert_eq!(pruned, chain);
}

/// Deleting a parent cascades to its `delegate` children, so retention must
/// not delete an old parent whose delegate child is not itself prunable.
#[test]
fn retention_keeps_an_old_parent_whose_delegate_child_is_not_prunable() {
    let home = TestHome::new();
    let store = home.store("default");
    let month = Duration::from_hours(30 * 24);
    let pinned_parent = session(&store);
    let pinned_child = child(&store, pinned_parent.id, LineageKind::Delegate);
    let live_parent = session(&store);
    let live_child = child(&store, live_parent.id, LineageKind::Delegate);
    for id in [pinned_parent.id, pinned_child, live_parent.id] {
        age(&home, id, 100);
    }
    store.set_pinned(pinned_child, true).unwrap();

    assert!(store.prune(month, true).unwrap().is_empty(), "dry run");
    assert!(store.prune(month, false).unwrap().is_empty());
    for id in [pinned_parent.id, pinned_child, live_parent.id, live_child] {
        store.session(id).unwrap();
    }
}

#[test]
fn dry_run_lists_exactly_what_prune_deletes() {
    let home = TestHome::new();
    let store = home.store("default");
    let month = Duration::from_hours(30 * 24);
    let parent = session(&store);
    let delegate = child(&store, parent.id, LineageKind::Delegate);
    let nested = child(&store, delegate, LineageKind::Delegate);
    let fork = child(&store, parent.id, LineageKind::Fork);
    for id in [parent.id, delegate, nested] {
        age(&home, id, 100);
    }

    let mut would = store.prune(month, true).unwrap();
    let mut pruned = store.prune(month, false).unwrap();
    would.sort();
    pruned.sort();
    let mut expected = vec![parent.id, delegate, nested];
    expected.sort();
    assert_eq!(would, expected, "each session listed once");
    assert_eq!(pruned, expected);
    let kept = store.session(fork).unwrap();
    assert!(
        kept.parent_deleted,
        "the recent fork is re-parented, not pruned"
    );
}

#[test]
fn idle_lineage_tips_are_auto_archived_and_resume_clears_it() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    let tip = child(&store, a.id, LineageKind::Compression);
    age(&home, a.id, 10);
    age(&home, tip, 10);
    let archived = store.auto_archive(Duration::from_hours(7 * 24)).unwrap();
    assert_eq!(archived, 1, "only the tip");
    assert_eq!(store.session(tip).unwrap().archived, Archived::Auto);
    assert_eq!(store.session(a.id).unwrap().archived, Archived::None);
    assert_eq!(store.reopen_session(tip).unwrap().archived, Archived::None);
}

#[test]
fn oversized_lineage_is_refused_for_resume_and_in_memory_export() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    let batch: Vec<NewMessage> = (0..6)
        .map(|n| NewMessage::new(Role::User, format!("m{n}")))
        .collect();
    store.append_messages(a.id, &batch).unwrap();
    let b = child(&store, a.id, LineageKind::Compression);
    store
        .append_messages(
            b,
            &batch[..1]
                .iter()
                .map(|m| NewMessage::new(m.role, "again"))
                .collect::<Vec<_>>(),
        )
        .unwrap();

    let err = store.check_resumable(a.id, 5).unwrap_err();
    assert_eq!(err.code().code, "session.too_large_to_resume");
    assert!(err.hint().unwrap().contains("/compress"));
    store.check_resumable(a.id, 7).unwrap();
    store.check_resumable(a.id, 0).unwrap();
    let export = store
        .messages_for_export(a.id, Visibility::Active, 5)
        .unwrap_err();
    assert_eq!(export.code().code, "session.too_large_to_export");
    assert_eq!(
        store
            .messages_for_export(a.id, Visibility::Active, 6)
            .unwrap()
            .len(),
        6
    );
    let mut streamed = 0;
    store
        .for_each_message(a.id, Visibility::All, |_| {
            streamed += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(streamed, 6, "streaming has no cap");
}

fn usage(session: SessionId, call: &str, input: u64) -> UsageEvent {
    UsageEvent {
        call_id: call.to_owned(),
        ts: jiff::Timestamp::now(),
        session_id: session,
        turn_id: None,
        parent_call_id: None,
        purpose: "main".to_owned(),
        purpose_detail: None,
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        service_tier: None,
        tokens: Tokens {
            input,
            output: 1,
            ..Tokens::default()
        },
        cost_usd: None,
        cost_status: CostStatus::Unknown,
        billing_source: "api_key:openai".to_owned(),
        request_hash: None,
        cache_hit: false,
    }
}

#[test]
fn continuation_usage_counts_in_the_lineage_total() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    store.record_usage(&usage(a.id, "a1", 1000)).unwrap();
    let b = child(&store, a.id, LineageKind::Compression);
    let mut priced = usage(b, "b1", 500);
    priced.cost_usd = Some(0.25);
    priced.cost_status = CostStatus::Exact;
    store.record_usage(&priced).unwrap();
    let d = child(&store, b, LineageKind::Delegate);
    store.record_usage(&usage(d, "d1", 7)).unwrap();

    let lineage = store.usage_totals(UsageScope::Lineage(a.id)).unwrap();
    assert_eq!(lineage.tokens.input, 1507);
    assert_eq!(lineage.calls, 3);
    assert_eq!(lineage.unpriced_calls, 2);
    assert!((lineage.cost_usd - 0.25).abs() < 1e-9);
    assert_eq!(store.usage_totals(UsageScope::Lineage(d)).unwrap(), lineage);
    assert_eq!(
        store
            .usage_totals(UsageScope::Session(b))
            .unwrap()
            .tokens
            .input,
        500
    );
    assert_eq!(store.usage_totals(UsageScope::Profile).unwrap().calls, 3);
}

#[test]
fn ledger_is_append_only_and_validates_billing_source() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = session(&store);
    store.record_usage(&usage(a.id, "a1", 1)).unwrap();
    let updated =
        raw(&home.database("default")).execute("UPDATE usage_ledger SET input_tokens = 0", []);
    assert!(updated.is_err(), "rows are never updated");
    let mut bad = usage(a.id, "a2", 1);
    bad.billing_source = "free".to_owned();
    assert_eq!(
        store.record_usage(&bad).unwrap_err().code().code,
        "store.constraint"
    );
}

#[test]
fn continue_skips_integration_sessions() {
    let home = TestHome::new();
    let store = home.store("default");
    let mut interactive = NewSession::new(Source::Cli);
    interactive.cwd = Some("/repo".to_owned());
    let mine = store.create_session(&interactive).unwrap();
    let mut tool = NewSession::new(Source::Cli);
    tool.cwd = Some("/repo".to_owned());
    tool.source_tag = Some("tool".to_owned());
    let integration = store.create_session(&tool).unwrap();
    store.record_activity(integration.id).unwrap();
    let mut elsewhere = NewSession::new(Source::Cli);
    elsewhere.cwd = Some("/other".to_owned());
    store.create_session(&elsewhere).unwrap();

    assert_eq!(store.continue_target("/repo").unwrap().unwrap().id, mine.id);
    assert!(store.continue_target("/nowhere").unwrap().is_none());
    let listed = store.list_sessions(&ListOptions::default()).unwrap();
    assert!(
        listed.iter().all(|s| s.id != integration.id),
        "hidden from listings"
    );
    let all = store
        .list_sessions(&ListOptions {
            all: true,
            ..ListOptions::default()
        })
        .unwrap();
    assert!(all.iter().any(|s| s.id == integration.id), "kept in --all");
    assert_eq!(
        store.session(integration.id).unwrap().source_tag.as_deref(),
        Some("tool")
    );
}

#[test]
fn invalid_source_tags_are_refused() {
    let home = TestHome::new();
    let store = home.store("default");
    for tag in ["", "Tool", "9lives", "has space", &"x".repeat(33)] {
        let mut new = NewSession::new(Source::Cli);
        new.source_tag = Some(tag.to_owned());
        let err = store.create_session(&new).unwrap_err();
        assert_eq!(err.code().code, "session.invalid_source_tag", "{tag:?}");
        assert_eq!(err.exit_code(), 2);
    }
    let mut ok = NewSession::new(Source::Cli);
    ok.source_tag = Some(pmt_store::ONESHOT_TAG.to_owned());
    store.create_session(&ok).unwrap();
}

#[test]
fn listing_honours_limit_pins_and_archive() {
    let home = TestHome::new();
    let store = home.store("default");
    let ids: Vec<SessionId> = (0..4).map(|_| session(&store).id).collect();
    store.set_pinned(ids[0], true).unwrap();
    store.set_archived(ids[1], Archived::User).unwrap();
    store.end_session(ids[2], EndReason::UserExit).unwrap();
    let limited = store
        .list_sessions(&ListOptions {
            limit: Some(2),
            ..ListOptions::default()
        })
        .unwrap();
    assert_eq!(limited.len(), 2);
    let pinned = store
        .list_sessions(&ListOptions {
            pinned_only: true,
            ..ListOptions::default()
        })
        .unwrap();
    assert_eq!(
        pinned.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    let default = store.list_sessions(&ListOptions::default()).unwrap();
    assert!(
        default.iter().all(|s| s.id != ids[1]),
        "archived hidden by default"
    );
}

/// Spec (cli) scenario "Per-terminal continue": session A was last used
/// from terminal 1, a newer session B from terminal 2; `-c` in terminal 1
/// picks A, a terminal that used neither picks the newest.
#[test]
fn continue_prefers_the_session_last_used_from_the_same_terminal() {
    let home = TestHome::new();
    let store = home.store("default");
    let mut new = NewSession::new(Source::Cli);
    new.cwd = Some("/repo".to_owned());
    let a = store.create_session(&new).unwrap();
    store.record_terminal(a.id, "tty1").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = store.create_session(&new).unwrap();
    store.record_terminal(b.id, "tty2").unwrap();
    store.record_activity(b.id).unwrap();

    let pick = |terminal| {
        store
            .continue_target_from("/repo", terminal)
            .unwrap()
            .unwrap()
            .id
    };
    assert_eq!(pick(Some("tty1")), a.id);
    assert_eq!(pick(Some("tty2")), b.id);
    assert_eq!(pick(Some("tty3")), b.id);
    assert_eq!(pick(None), b.id);
    // A later use from terminal 1 moves the preference there.
    store.record_terminal(b.id, "tty1").unwrap();
    assert_eq!(pick(Some("tty2")), b.id, "no session left on tty2: newest");
}
