//! Offline recovery, journal mode, prompt refresh, retitling, the low-disk
//! prune prompt and the attachment storage location (spec:
//! session-storage, Offline recovery and journal mode; Prompt refresh and
//! retitling; Low-disk prune prompt; Attachment storage location).

mod common;

use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{NewMessage, NewSession, Role, Source, Store, TitleSource};
use prometheus::chat::autoprune::{self, Answer};
use serde_json::Value;

use common::{PNG, append_config, mock_home, newest_session, ok, run, stderr};

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(
        profile,
        &Overrides::parse::<&str>(&[]).unwrap(),
        Launch::Other,
    )
    .unwrap()
}

fn profile_dir(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default")
}

#[test]
fn recover_copies_every_row_and_swaps_in_keeping_the_original() {
    let home = mock_home();
    ok(&run(home.path(), &["-z", "hello there"]));
    let id = newest_session(home.path());
    let out = run(home.path(), &["sessions", "recover", "--yes"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("sessions: 1 recovered, 0 lost"), "{err}");
    assert!(err.contains("messages: 2 recovered"), "{err}");
    assert!(
        profile_dir(home.path())
            .join("state.db.pre-recover")
            .exists()
    );
    let shown: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "sessions", "show", &id]))).unwrap();
    assert_eq!(shown["session"]["message_count"], 2);
    let found = ok(&run(home.path(), &["sessions", "search", "hello"]));
    assert!(found.contains(&id), "indexes were rebuilt: {found}");
}

#[cfg(feature = "gateway")]
#[test]
fn recover_is_refused_while_a_daemon_serves_the_profile() {
    // Scenario: Recover refused while open.
    let home = mock_home();
    ok(&run(home.path(), &["-z", "hello"]));
    let daemon = common::daemon::start(home.path(), &[]);
    let pid = common::daemon::record(home.path()).process.pid;
    let out = run(home.path(), &["sessions", "recover", "--yes"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains(&pid.to_string()), "{}", stderr(&out));
    let leftovers: Vec<_> = std::fs::read_dir(profile_dir(home.path()))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("state.recovered")
        })
        .collect();
    assert!(leftovers.is_empty(), "nothing was written");
    drop(daemon);
}

#[test]
fn the_journal_mode_is_recorded_in_the_store_and_reapplied_on_open() {
    let home = mock_home();
    ok(&run(home.path(), &["-z", "hello"]));
    ok(&run(home.path(), &["sessions", "journal-mode", "delete"]));
    // Any later open applies it although storage.journal_mode is wal.
    ok(&run(home.path(), &["sessions", "list"]));
    let db = rusqlite::Connection::open(profile_dir(home.path()).join("state.db")).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    drop(db);
    ok(&run(home.path(), &["sessions", "journal-mode", "wal"]));
    let db = rusqlite::Connection::open(profile_dir(home.path()).join("state.db")).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn a_refreshed_session_rebuilds_its_prompt_once_at_its_next_turn() {
    let home = mock_home();
    ok(&run(home.path(), &["-z", "first"]));
    let id = newest_session(home.path());
    let marked: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "refresh-prompts", &id],
    )))
    .unwrap();
    assert_eq!(marked["marked"][0], id.as_str());
    ok(&run(home.path(), &["chat", "-r", &id, "-z", "second"]));
    ok(&run(home.path(), &["chat", "-r", &id, "-z", "third"]));
    let trajectory = ok(&run(
        home.path(),
        &["trajectory", "show", &id, "--format", "jsonl"],
    ));
    let refreshed = trajectory
        .lines()
        .filter(|l| l.contains("\"prompt.rebuilt\"") && l.contains("refresh-prompts"))
        .count();
    assert_eq!(refreshed, 1, "{trajectory}");
}

#[test]
fn retitle_keeps_user_titles_and_renames_automatic_ones() {
    // Scenario: User titles kept.
    let home = mock_home();
    std::fs::write(
        profile_dir(home.path()).join("titles.toml"),
        "[[rule]]\nuser_regex = \"(?s).*\"\ntext = \"Spreadsheet help\"\n",
    )
    .unwrap();
    append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"titles.toml\"\n",
    );
    let store = Store::open(&scope(home.path())).unwrap();
    let mut mine = NewSession::new(Source::Cli);
    mine.title = Some("My notes".to_owned());
    mine.title_source = TitleSource::User;
    let mine = store.create_session(&mine).unwrap().id;
    let mut auto = NewSession::new(Source::Cli);
    auto.title = Some("Untitled chat".to_owned());
    auto.title_source = TitleSource::Derived;
    let auto = store.create_session(&auto).unwrap().id;
    for id in [mine, auto] {
        store
            .append_message(id, &NewMessage::new(Role::User, "sum column B please"))
            .unwrap();
    }
    drop(store);
    let out = run(home.path(), &["sessions", "retitle", "--all"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let store = Store::open(&scope(home.path())).unwrap();
    assert_eq!(
        store.session(mine).unwrap().title.as_deref(),
        Some("My notes")
    );
    let renamed = store.session(auto).unwrap();
    assert_eq!(renamed.title.as_deref(), Some("Spreadsheet help"));
    assert_eq!(renamed.title_source, Some(TitleSource::Llm));
}

#[test]
fn the_low_disk_prompt_answered_later_waits_a_week() {
    // Scenario: Later postpones a week.
    let home = mock_home();
    // Any volume is "low" against this threshold.
    append_config(
        home.path(),
        "\n[sessions]\nlow_disk_bytes = 9223372036854775807\n",
    );
    let scope = scope(home.path());
    let store = Store::open(&scope).unwrap();
    let today: Timestamp = "2026-11-07T10:00:00Z".parse().unwrap();
    assert!(autoprune::due(&scope, &store, today).unwrap());
    assert!(
        !autoprune::due(&scope, &store, today).unwrap(),
        "at most once a day"
    );
    autoprune::apply(&scope, &store, Answer::Later, today).unwrap();
    let tomorrow = today + SignedDuration::from_hours(25);
    assert!(!autoprune::due(&scope, &store, tomorrow).unwrap());
    let next_week = today + SignedDuration::from_hours(24 * 8);
    assert!(autoprune::due(&scope, &store, next_week).unwrap());

    autoprune::apply(&scope, &store, Answer::On, next_week).unwrap();
    let config = std::fs::read_to_string(profile_dir(home.path()).join("config.toml")).unwrap();
    assert!(config.contains("retention_secs = 7776000"), "{config}");
    let status = autoprune::command(&scope, &store, "status").unwrap();
    assert!(status.contains("free space"), "{status}");
}

#[test]
fn the_low_disk_prompt_answered_off_never_asks_again() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[sessions]\nlow_disk_bytes = 9223372036854775807\n",
    );
    let scope = scope(home.path());
    let store = Store::open(&scope).unwrap();
    let now = Timestamp::now();
    assert!(autoprune::due(&scope, &store, now).unwrap());
    autoprune::apply(&scope, &store, Answer::Off, now).unwrap();
    let much_later = now + SignedDuration::from_hours(24 * 60);
    assert!(!autoprune::due(&scope, &store, much_later).unwrap());
}

#[test]
fn a_workspace_attachment_is_copied_beside_the_blob() {
    // Scenario: Workspace copy (an image: the CLI attaches images).
    let home = mock_home();
    append_config(home.path(), "\n[attachments]\nstorage = \"workspace\"\n");
    let repo = home.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let shot = home.path().join("report.png");
    std::fs::write(&shot, PNG).unwrap();
    let out = common::command(home.path())
        .current_dir(&repo)
        .args(["-z", "describe", "--image", shot.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = newest_session(home.path());
    let copy = repo
        .join(".prometheus/attachments")
        .join(&id)
        .join("report.png");
    assert_eq!(std::fs::read(&copy).unwrap(), PNG);
    let store = Store::open(&scope(home.path())).unwrap();
    let blobs = store.session_blob_refs(id.parse().unwrap()).unwrap();
    assert_eq!(blobs.len(), 1, "the blob store stays canonical");
    assert_eq!(store.get_blob(&blobs[0]).unwrap(), PNG);
}
