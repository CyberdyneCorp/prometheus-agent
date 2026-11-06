//! `/worktree` and the `worktree.*` methods on daemon sessions, and a
//! messaging session's tree at its end (spec: git-worktrees, In-session
//! worktree command; Exit disposition).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{daemon, mock_home};
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository with one commit and no remote, inside `home`.
fn repo(home: &Path) -> PathBuf {
    let work = home.canonicalize().unwrap().join("repo");
    init(&work);
    work
}

/// Makes `work` a repository with one commit and no remote.
fn init(work: &Path) {
    std::fs::create_dir_all(work).unwrap();
    git(work, &["init", "-q", "-b", "main"]);
    git(work, &["config", "user.name", "t"]);
    git(work, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-q", "-m", "init"]);
}

fn prompt_hash(home: &Path, session: &str) -> Option<String> {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT system_prompt_hash FROM sessions WHERE id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

async fn submit(client: &pmt_rpc::client::Client, session: &str, text: &str) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
}

async fn finished(inbox: &mut pmt_rpc::client::Inbox) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("a turn finishes within 30 s")
            .expect("the connection is open");
        if event.event == "turn.finished" {
            return;
        }
    }
}

fn exposed(home: &Path, id: &str) -> Vec<String> {
    common::ok(&common::run(home, &["tools", "list", "--session", id]))
        .lines()
        .filter(|l| l.contains(" exposed"))
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

const SCRIPT: &str = r#"
[[rule]]
user = "where are you"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "pwd" }

[[rule]]
tool_result_regex = "/repo/\\.worktrees/exp"
text = "terminal in the exp worktree"

[[rule]]
tool_result_regex = "(?s).*"
text = "terminal elsewhere"

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Prompt cache preserved.
#[test]
fn moving_into_a_new_worktree_keeps_the_prompt_and_tools_and_moves_the_terminal() {
    let home = mock_home();
    let work = repo(home.path());
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let created = client
            .call(
                "session.create",
                json!({"cwd": work.to_string_lossy(), "setup": {"mock_script": script.to_string_lossy()}}),
            )
            .await
            .unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        submit(&client, &session, "first").await;
        finished(&mut inbox).await;
        let before = prompt_hash(home.path(), &session).expect("the first turn set it");
        let tools_before = exposed(home.path(), &session);

        let moved = client
            .call(
                "command.dispatch",
                json!({"session_id": session, "name": "worktree", "args": "new exp"}),
            )
            .await
            .unwrap();
        let tree = work.join(".worktrees/exp");
        assert!(moved.to_string().contains(&*tree.to_string_lossy()), "{moved}");

        submit(&client, &session, "where are you").await;
        finished(&mut inbox).await;
        let transcript = client
            .call("session.transcript", json!({"session_id": session}))
            .await
            .unwrap()
            .to_string();
        assert!(transcript.contains("terminal in the exp worktree"), "{transcript}");
        assert!(
            transcript.contains("the session's working directory is now"),
            "the move is announced in the next tool result: {transcript}"
        );
        assert_eq!(prompt_hash(home.path(), &session).as_deref(), Some(before.as_str()));
        assert_eq!(exposed(home.path(), &session), tools_before);

        let status = client
            .call("worktree.status", json!({"session_id": session}))
            .await
            .unwrap();
        assert_eq!(status["tree"]["name"], "exp", "{status}");
        assert_eq!(status["tree"]["branch"], "prometheus/exp", "{status}");
        let listed = client
            .call("worktree.list", json!({"session_id": session}))
            .await
            .unwrap();
        assert_eq!(listed["trees"][0]["name"], "exp", "{listed}");
        let pruned = client
            .call("worktree.prune", json!({"session_id": session, "dry_run": true}))
            .await
            .unwrap();
        assert_eq!(pruned["decisions"][0]["reason"], "live-owner", "{pruned}");
        let other = client
            .call("worktree.new", json!({"session_id": session, "name": "second"}))
            .await
            .unwrap();
        assert_eq!(other["tree"]["branch"], "prometheus/second", "{other}");
        assert!(work.join(".worktrees/second").is_dir());
    });
}

/// Scenario: Gateway session.
#[test]
fn a_messaging_sessions_dirty_tree_is_kept_without_a_prompt_when_it_ends() {
    use common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
    let home = gateway_home_with("[[rule]]\nrepeat = true\ntext = \"done\"\n", "");
    // Messaging sessions work in the profile's workspace: a repository.
    let work = home
        .path()
        .canonicalize()
        .unwrap()
        .join("profiles/default/workspace");
    init(&work);
    let _daemon = Daemon::start(home.path());
    send(home.path(), "default", "alice", "hello");
    wait_said(home.path(), "default", "done");
    send(home.path(), "default", "alice", "/worktree new lane");
    wait_said(home.path(), "default", "working directory is now");
    let tree = work.join(".worktrees/lane");
    std::fs::write(tree.join("draft.txt"), "unfinished").unwrap();
    let before = texts(home.path(), "default").len();
    send(home.path(), "default", "alice", "/new");
    wait("the session was reset", Duration::from_secs(30), || {
        texts(home.path(), "default").len() > before
    });
    send(home.path(), "default", "alice", "hello again");
    wait("the next session answered", Duration::from_secs(30), || {
        texts(home.path(), "default")
            .iter()
            .filter(|t| *t == "done")
            .count()
            >= 2
    });
    let said = texts(home.path(), "default");
    assert!(
        !said
            .iter()
            .any(|t| t.contains("[k]eep") || t.contains("archive and remove")),
        "{said:?}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("draft.txt")).unwrap(),
        "unfinished"
    );
    git(
        &work,
        &["rev-parse", "--verify", "refs/heads/prometheus/lane"],
    );
}
