//! Session forking from the CLI and portable export/import between two
//! homes (spec: session-storage, Session forking API; Portable export and
//! import).

mod common;

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{
    LineageKind, NewMessage, NewSession, Role, Source, Store, TitleSource, Visibility,
};
use serde_json::Value;

use common::{ok, run, stderr, temp_home};

fn store(home: &Path) -> Store {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(
        profile,
        &Overrides::parse::<&str>(&[]).unwrap(),
        Launch::Other,
    )
    .unwrap();
    Store::open(&scope).unwrap()
}

/// Session "Atlas" with ten messages (m1..m10) and an attachment blob on
/// the first one.
fn seed(home: &Path) -> (SessionId, Vec<MessageId>) {
    let store = store(home);
    let mut new = NewSession::new(Source::Cli);
    new.title = Some("Atlas".to_owned());
    new.title_source = TitleSource::User;
    let id = store.create_session(&new).unwrap().id;
    let rows: Vec<NewMessage> = (1..=10)
        .map(|i| {
            let role = if i % 2 == 1 {
                Role::User
            } else {
                Role::Assistant
            };
            NewMessage::new(role, format!("message {i}"))
        })
        .collect();
    let ids = rows.iter().map(|m| m.id).collect();
    store.append_messages(id, &rows).unwrap();
    let blob = store.put_blob(b"%PDF-1.7 attachment").unwrap();
    store
        .reference_blob(&blob, "message", &rows[0].id.to_string())
        .unwrap();
    (id, ids)
}

fn json(out: &std::process::Output) -> Value {
    serde_json::from_str(&ok(out)).unwrap()
}

#[test]
fn a_fork_keeps_the_messages_up_to_the_chosen_one_and_the_tree_lists_it() {
    // Scenarios: Fork from an earlier message; Tree lists forks and
    // continuations.
    let home = temp_home();
    let (a, messages) = seed(home.path());
    let forked = json(&run(
        home.path(),
        &[
            "sessions",
            "fork",
            &a.to_string(),
            "--at",
            &messages[3].to_string(),
            "--json",
        ],
    ));
    let f: SessionId = forked["session"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(forked["session"]["title"], "Atlas #2");
    assert_eq!(forked["session"]["lineage_kind"], "fork");
    let s = store(home.path());
    let copies = s.messages(f, Visibility::Active).unwrap();
    let origins: Vec<String> = copies
        .iter()
        .map(|m| {
            m.metadata.as_ref().unwrap()["origin_message_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let expected: Vec<String> = messages[..4].iter().map(ToString::to_string).collect();
    assert_eq!(origins, expected);
    assert_eq!(s.messages(a, Visibility::Active).unwrap().len(), 10);

    let mut continuation = NewSession::child(Source::Cli, a, LineageKind::Compression);
    continuation.title = None;
    let b = s.create_session(&continuation).unwrap().id;
    let tree = json(&run(
        home.path(),
        &["sessions", "tree", &a.to_string(), "--json"],
    ));
    let kinds: Vec<(String, String)> = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        kinds.contains(&(f.to_string(), "fork".to_owned())),
        "{tree}"
    );
    assert!(
        kinds.contains(&(b.to_string(), "compression".to_owned())),
        "{tree}"
    );
    let siblings = json(&run(
        home.path(),
        &["sessions", "siblings", &f.to_string(), "--json"],
    ));
    let ids: Vec<&str> = siblings["siblings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![b.to_string().as_str()]);
}

#[test]
fn an_export_round_trips_between_two_homes_with_its_blobs() {
    let source = temp_home();
    let (a, _) = seed(source.path());
    let file = source.path().join("atlas.jsonl");
    ok(&run(
        source.path(),
        &[
            "sessions",
            "export",
            &a.to_string(),
            "--with-blobs",
            "-o",
            file.to_str().unwrap(),
        ],
    ));

    let target = temp_home();
    ok(&run(target.path(), &["config", "path"]));
    let imported = json(&run(
        target.path(),
        &["sessions", "import", file.to_str().unwrap(), "--json"],
    ));
    assert_eq!(imported["sessions"][0], a.to_string().as_str());
    let s = store(target.path());
    let session = s.session(a).unwrap();
    assert_eq!(session.title.as_deref(), Some("Atlas"));
    let messages = s.messages(a, Visibility::Active).unwrap();
    assert_eq!(messages.len(), 10);
    assert_eq!(messages[9].content.as_deref(), Some("message 10"));
    let refs = s.message_blob_refs(a).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(s.get_blob(&refs[0].1).unwrap(), b"%PDF-1.7 attachment");

    // Scenario: Collision without remap.
    let before = s
        .list_sessions(&pmt_store::ListOptions::default())
        .unwrap()
        .len();
    let again = run(
        target.path(),
        &["sessions", "import", file.to_str().unwrap()],
    );
    assert!(!again.status.success());
    let err = stderr(&again);
    assert!(
        err.contains("import.id_collision") || err.contains("already exist"),
        "{err}"
    );
    assert!(err.contains(&a.to_string()), "{err}");
    assert_eq!(
        s.list_sessions(&pmt_store::ListOptions::default())
            .unwrap()
            .len(),
        before,
        "nothing was written"
    );

    let remapped = json(&run(
        target.path(),
        &[
            "sessions",
            "import",
            file.to_str().unwrap(),
            "--remap-ids",
            "--json",
        ],
    ));
    let new: SessionId = remapped["sessions"][0].as_str().unwrap().parse().unwrap();
    assert_ne!(new, a);
    assert_eq!(s.messages(new, Visibility::Active).unwrap().len(), 10);
    assert_eq!(s.session(new).unwrap().title.as_deref(), Some("Atlas #2"));
}

#[test]
fn an_invalid_file_writes_nothing() {
    let home = temp_home();
    let (a, _) = seed(home.path());
    let file = home.path().join("bad.jsonl");
    ok(&run(
        home.path(),
        &[
            "sessions",
            "export",
            &a.to_string(),
            "-o",
            file.to_str().unwrap(),
        ],
    ));
    let text = std::fs::read_to_string(&file).unwrap();
    let broken = text.replacen("\"role\":\"user\"", "\"role\":\"wizard\"", 1);
    std::fs::write(&file, broken).unwrap();
    let target = temp_home();
    ok(&run(target.path(), &["config", "path"]));
    let out = run(
        target.path(),
        &["sessions", "import", file.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("nothing was imported"),
        "{}",
        stderr(&out)
    );
    assert!(store(target.path()).session(a).is_err());
}

#[cfg(feature = "gateway")]
#[test]
fn the_daemon_serves_tree_and_siblings_over_rpc() {
    // The forking API exposed identically over the gateway RPC.
    let home = common::mock_home();
    let (a, messages) = seed(home.path());
    let daemon = common::daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, _inbox) = common::daemon::connect(home.path(), "default").await;
        let a = a.to_string();
        client
            .call("session.attach", serde_json::json!({"session_id": a}))
            .await
            .unwrap();
        let fork = client
            .call(
                "session.fork",
                serde_json::json!({"session_id": a, "at_message_id": messages[3].to_string()}),
            )
            .await
            .unwrap();
        let f = fork["session_id"].as_str().unwrap().to_owned();
        let tree = client
            .call("session.tree", serde_json::json!({"session_id": a}))
            .await
            .unwrap();
        assert_eq!(tree["nodes"][0]["session_id"], f.as_str());
        assert_eq!(tree["nodes"][0]["kind"], "fork");
        assert_eq!(tree["nodes"][0]["title"], "Atlas #2");
        client
            .call("session.attach", serde_json::json!({"session_id": f}))
            .await
            .unwrap();
        let siblings = client
            .call("session.siblings", serde_json::json!({"session_id": f}))
            .await
            .unwrap();
        assert_eq!(siblings["nodes"], serde_json::json!([]));
    });
    drop(daemon);
}
