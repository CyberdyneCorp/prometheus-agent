//! `prometheus sessions` against the built binary (spec: session-storage,
//! Session management CLI; Export formats and save command). Text output
//! is compared with snapshots in `tests/snapshots/` after ids and times
//! are replaced by placeholders; `UPDATE_SNAPSHOTS=1` rewrites them.

mod common;

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{NewMessage, NewSession, Role, Source, Store, TitleSource, Visibility};
use serde_json::Value;

use common::{ok, run, stderr, stdout, temp_home};

/// The profile with storage redaction off, so the seeded transcript keeps
/// its credential-shaped text and the export's own `--redact` / `/save
/// ... redact` is what the tests below exercise.
fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let raw = Overrides::parse(&["security.redact_secrets=false".to_owned()]).unwrap();
    ProfileScope::open(profile, &raw, Launch::Other).unwrap()
}

struct Seeded {
    refactor: SessionId,
    deploy: SessionId,
    question: MessageId,
}

/// Two titled sessions with a short transcript each, and one integration
/// session.
fn seed(home: &Path) -> Seeded {
    let store = Store::open(&scope(home)).unwrap();
    let mut new = NewSession::new(Source::Cli);
    new.title = Some("Refactor".to_owned());
    new.title_source = TitleSource::User;
    new.cwd = Some("/repo".to_owned());
    let refactor = store.create_session(&new).unwrap().id;
    let question = NewMessage::new(Role::User, "Split the parser into modules");
    store
        .append_messages(
            refactor,
            &[
                question.clone(),
                NewMessage::new(
                    Role::Assistant,
                    "Done: the parser now lives in parse/ with one module per stage.",
                ),
            ],
        )
        .unwrap();
    new.title = Some("Deploy notes".to_owned());
    let deploy = store.create_session(&new).unwrap().id;
    store
        .append_message(
            deploy,
            &NewMessage::new(
                Role::User,
                "Our kubernetes token is sk-live-abcdef1234567890",
            ),
        )
        .unwrap();
    let mut tool = NewSession::new(Source::Cli);
    tool.source_tag = Some("tool".to_owned());
    store.create_session(&tool).unwrap();
    Seeded {
        refactor,
        deploy,
        question: question.id,
    }
}

/// Ids and times replaced by numbered placeholders, in order of
/// appearance.
fn normalize(text: &str) -> String {
    let mut ids: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next_id = ["ses_", "msg_"].iter().filter_map(|p| rest.find(p)).min();
        let Some(at) = next_id else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        let id = &rest[at..(at + 30).min(rest.len())];
        let n = ids.iter().position(|seen| seen == id).unwrap_or_else(|| {
            ids.push(id.to_owned());
            ids.len() - 1
        });
        let _ = write!(out, "<{}{}>", &id[..3].to_uppercase(), n + 1);
        rest = &rest[(at + 30).min(rest.len())..];
    }
    times(&out)
}

/// `YYYY-MM-DD HH:MM` and RFC 3339 times as `<TIME>`.
fn times(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let is_date = i + 16 <= chars.len()
            && chars[i..i + 4].iter().all(char::is_ascii_digit)
            && chars[i + 4] == '-'
            && chars[i + 7] == '-'
            && matches!(chars[i + 10], ' ' | 'T');
        if !is_date {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut end = i + 16;
        while end < chars.len()
            && (chars[end].is_ascii_digit() || matches!(chars[end], ':' | '.' | 'Z'))
        {
            end += 1;
        }
        out.push_str("<TIME>");
        i = end;
    }
    out
}

fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    let actual = normalize(actual);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

fn json(out: &std::process::Output) -> Value {
    serde_json::from_str(&ok(out)).unwrap()
}

#[test]
fn list_show_and_search_match_their_snapshots() {
    let home = temp_home();
    seed(home.path());
    snapshot(
        "sessions-list.txt",
        &ok(&run(home.path(), &["sessions", "list"])),
    );
    snapshot(
        "sessions-list-all.txt",
        &ok(&run(home.path(), &["sessions", "list", "--all"])),
    );
    snapshot(
        "sessions-show.txt",
        &ok(&run(home.path(), &["sessions", "show", "Refactor"])),
    );
    snapshot(
        "sessions-search.txt",
        &ok(&run(home.path(), &["sessions", "search", "parser"])),
    );
}

#[test]
fn json_listing_has_the_session_fields() {
    let home = temp_home();
    seed(home.path());
    let doc = json(&run(
        home.path(),
        &["sessions", "list", "--json", "--limit", "5"],
    ));
    assert_eq!(doc["schema"], "prometheus.sessions.v1");
    let sessions = doc["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "the integration session is hidden");
    for session in sessions {
        for key in ["id", "title", "source", "started_at", "message_count"] {
            assert!(session.get(key).is_some(), "{key} in {session}");
        }
    }
    let limited = json(&run(
        home.path(),
        &["sessions", "list", "--json", "--limit", "1", "--all"],
    ));
    assert_eq!(limited["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn unknown_session_exits_66_with_a_json_error() {
    let home = temp_home();
    seed(home.path());
    let out = run(home.path(), &["sessions", "show", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(66));
    assert!(stdout(&out).is_empty());
    let err: Value = serde_json::from_str(&stderr(&out)).unwrap();
    assert_eq!(err["error"]["code"], "session.not_found");
}

#[test]
fn search_json_and_hostile_queries() {
    let home = temp_home();
    seed(home.path());
    let doc = json(&run(
        home.path(),
        &["sessions", "search", "kubernetes", "--json"],
    ));
    assert_eq!(doc["schema"], "prometheus.search.v1");
    assert_eq!(doc["mode"], "word");
    assert_eq!(doc["hits"][0]["role"], "user");
    assert!(
        doc["hits"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("«kubernetes»")
    );
    for hostile in ["chat-send", "\"open", "NOT", "a OR", "(x"] {
        ok(&run(home.path(), &["sessions", "search", hostile]));
    }
}

#[test]
fn rename_pin_archive_tree_and_stats() {
    let home = temp_home();
    let seeded = seed(home.path());
    let id = seeded.refactor.to_string();
    ok(&run(
        home.path(),
        &["sessions", "rename", &id, "Parser split"],
    ));
    let doc = json(&run(
        home.path(),
        &["sessions", "show", "Parser split", "--json"],
    ));
    assert_eq!(doc["schema"], "prometheus.session.v1");
    assert_eq!(doc["session"]["id"], id.as_str());
    let taken = run(home.path(), &["sessions", "rename", &id, "Deploy notes"]);
    assert_eq!(taken.status.code(), Some(65));
    assert!(stderr(&taken).contains("Title 'Deploy notes' is already in use"));
    ok(&run(home.path(), &["sessions", "pin", &id]));
    ok(&run(home.path(), &["sessions", "archive", &id]));
    let listed = json(&run(home.path(), &["sessions", "list", "--json"]));
    assert_eq!(
        listed["sessions"].as_array().unwrap().len(),
        1,
        "archived hidden"
    );
    ok(&run(home.path(), &["sessions", "unarchive", &id]));
    let tree = json(&run(home.path(), &["sessions", "tree", &id, "--json"]));
    assert_eq!(tree["schema"], "prometheus.session_tree.v1");
    let stats = json(&run(home.path(), &["sessions", "stats", &id, "--json"]));
    assert_eq!(stats["schema"], "prometheus.session_stats.v1");
    assert_eq!(stats["active_lineage_messages"], 2);
}

#[test]
fn exports_in_every_format() {
    let home = temp_home();
    let seeded = seed(home.path());
    let id = seeded.refactor.to_string();
    let jsonl = ok(&run(home.path(), &["sessions", "export", &id]));
    let first: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(
        first,
        serde_json::json!({"format": "prometheus.sessions", "version": 1})
    );
    let kinds: Vec<String> = jsonl
        .lines()
        .skip(1)
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["session", "message", "message", "lineage"]);
    snapshot(
        "sessions-export.md",
        &ok(&run(
            home.path(),
            &["sessions", "export", &id, "--format", "md"],
        )),
    );
    let html = ok(&run(
        home.path(),
        &["sessions", "export", &id, "--format", "html"],
    ));
    assert!(html.starts_with("<!doctype html>") && html.contains("Split the parser"));
    assert!(!html.contains("http://") && !html.contains("https://") && !html.contains("src="));
    let prompts = ok(&run(
        home.path(),
        &["sessions", "export", &id, "--format", "prompts"],
    ));
    assert_eq!(prompts.lines().count(), 1);
    let trace = ok(&run(
        home.path(),
        &["sessions", "export", &id, "--format", "trace"],
    ));
    let types: Vec<String> = trace
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        types,
        ["session.start", "user.message", "assistant.message"]
    );
}

#[test]
fn redacted_export_to_a_file_contains_no_secret() {
    let home = temp_home();
    let seeded = seed(home.path());
    let file = home.path().join("deploy.md");
    let id = seeded.deploy.to_string();
    let file_arg = file.to_str().unwrap();
    ok(&run(
        home.path(),
        &[
            "sessions", "export", &id, "--format", "md", "--redact", "-o", file_arg,
        ],
    ));
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("sk-live-abcdef1234567890"), "{text}");
    assert!(text.contains("[REDACTED]"));
    let again = run(home.path(), &["sessions", "export", &id, "-o", file_arg]);
    assert_eq!(
        again.status.code(),
        Some(73),
        "existing file is not overwritten"
    );
}

#[test]
fn delete_asks_for_confirmation_and_keeps_nothing() {
    let home = temp_home();
    let seeded = seed(home.path());
    let id = seeded.deploy.to_string();
    let refused = run(home.path(), &["sessions", "delete", &id]);
    assert_eq!(refused.status.code(), Some(4));
    ok(&run(home.path(), &["sessions", "show", &id]));
    let spill = home
        .path()
        .join("profiles/default/cache/spillover")
        .join(&id);
    std::fs::create_dir_all(&spill).unwrap();
    std::fs::write(spill.join("call_1.txt"), "oversized result").unwrap();
    ok(&run(home.path(), &["sessions", "delete", &id, "--yes"]));
    assert!(
        !spill.exists(),
        "the session's spillover directory is removed"
    );
    assert_eq!(
        run(home.path(), &["sessions", "show", &id]).status.code(),
        Some(66)
    );
}

#[test]
fn prune_dry_run_counts_and_prune_deletes() {
    let home = temp_home();
    let seeded = seed(home.path());
    let db = home.path().join("profiles/default/state.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE sessions SET ended_at = 1, end_reason = 'user_exit', last_activity_at = 1 WHERE id = ?1",
            [seeded.deploy.to_string()],
        )
        .unwrap();
    let dry = ok(&run(
        home.path(),
        &["sessions", "prune", "--older-than", "30", "--dry-run"],
    ));
    assert_eq!(dry.trim(), "would delete 1 session(s)");
    ok(&run(
        home.path(),
        &["sessions", "show", &seeded.deploy.to_string()],
    ));
    let pruned = ok(&run(
        home.path(),
        &["sessions", "prune", "--older-than", "30", "--yes"],
    ));
    assert_eq!(pruned.trim(), "deleted 1 session(s)");
    assert_eq!(
        run(
            home.path(),
            &["sessions", "show", &seeded.deploy.to_string()]
        )
        .status
        .code(),
        Some(66)
    );
}

#[test]
fn rewind_hides_and_restore_brings_back() {
    let home = temp_home();
    let seeded = seed(home.path());
    let id = seeded.refactor.to_string();
    ok(&run(home.path(), &["sessions", "rewind", &id]));
    let active = |home: &Path| {
        Store::open(&scope(home))
            .unwrap()
            .messages(seeded.refactor, Visibility::Active)
            .unwrap()
            .len()
    };
    assert_eq!(active(home.path()), 0);
    let question = seeded.question.to_string();
    ok(&run(
        home.path(),
        &["sessions", "rewind", &id, "--restore", &question],
    ));
    assert_eq!(active(home.path()), 2);
    let invalid = run(
        home.path(),
        &[
            "sessions",
            "rewind",
            &id,
            "--to",
            &MessageId::new().to_string(),
        ],
    );
    assert_eq!(invalid.status.code(), Some(65));
}

#[test]
fn repair_and_vacuum_run() {
    let home = temp_home();
    seed(home.path());
    let repaired = run(home.path(), &["sessions", "repair", "--indexes"]);
    ok(&repaired);
    assert!(stderr(&repaired).contains("rebuilt the search indexes (3 messages)"));
    ok(&run(home.path(), &["sessions", "vacuum"]));
    assert_eq!(
        run(home.path(), &["sessions", "repair"]).status.code(),
        Some(2)
    );
}

#[test]
fn redacted_save_contains_no_secret() {
    let home = temp_home();
    let seeded = seed(home.path());
    let scope = scope(home.path());
    let store = Store::open(&scope).unwrap();
    let cwd = home.path();

    let usage = prometheus::slash_handlers::save(&scope, &store, seeded.deploy, "", cwd).unwrap();
    assert!(usage.starts_with("usage: /save"), "{usage}");

    let saved =
        prometheus::slash_handlers::save(&scope, &store, seeded.deploy, "md notes.md redact", cwd)
            .unwrap();
    assert!(saved.ends_with("notes.md"), "{saved}");
    let text = std::fs::read_to_string(cwd.join("notes.md")).unwrap();
    assert!(!text.contains("sk-live-abcdef1234567890"), "{text}");
    assert!(text.contains("[REDACTED]"));

    let plain =
        prometheus::slash_handlers::save(&scope, &store, seeded.deploy, "html", cwd).unwrap();
    let path = plain.strip_prefix("saved ").unwrap();
    assert!(
        Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("deploy-notes-")
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("sk-live-abcdef1234567890"),
        "unredacted by default"
    );
}
