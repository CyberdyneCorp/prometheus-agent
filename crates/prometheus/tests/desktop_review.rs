//! The git review pane, simple mode, the memory graph and HUD mode end to
//! end (spec: desktop-app, Git review pane; Interface modes and layouts;
//! Memory graph; HUD mode): the page in the desktop browser harness
//! against the real daemon, driving `web/e2e/desktop-review.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::path::Path;

use common::dashboard::playwright;
use common::desktop::Setup;
use common::{append_config, mock_home, ok, run};
use pmt_desktop::hud::{Bounds, WindowBelow};

const SCRIPT: &str = r#"
[[rule]]
user = "edit two files"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "printf 'one\\n' > one.txt && printf 'two\\n' > two.txt && git add one.txt && git commit -q -m 'turn commit'" }

[[rule]]
user = "patch the readme"
reasoning = "pondering the patch"
[[rule.tool_calls]]
name = "patch"
arguments = { path = "README.md", old_string = "hello", new_string = "hello world" }

[[rule]]
user = "patch the readme again"
reasoning = "pondering again"
[[rule.tool_calls]]
name = "patch"
arguments = { path = "README.md", old_string = "world", new_string = "there" }

[[rule]]
repeat = true
text = "ok"
"#;

struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository the turns work in, with a committed README.
fn repository(home: &Path) -> std::path::PathBuf {
    let repo = common::user_home(home).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// The stored system prompt tiers of `session`.
fn prompt_of(home: &Path, session: &str) -> String {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT tiers FROM session_prompts WHERE session_id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

/// Scenario: Last-turn scope. Scenario: Revert needs confirmation.
/// Scenario: Share code is content-free. Scenario: Deleting a memory
/// mid-session. Scenario: Asking about the app underneath. Scenario: Snap
/// on native Wayland.
#[test]
fn desktop_review_pane_memory_graph_and_hud() {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), SCRIPT).unwrap();
    append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[approvals]\nmode = \"off\"\n",
    );
    ok(&run(
        home.path(),
        &["memory", "add", "the secret recipe uses saffron"],
    ));
    let repo = repository(home.path());
    let _stop = Stop(home.path());
    let below = WindowBelow {
        app: "Firefox".to_owned(),
        title: "Invoice 42".to_owned(),
        bounds: Bounds {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        },
    };
    let harness = common::desktop::start_with(
        home.path(),
        Setup {
            below: Some(below),
            ..Setup::default()
        },
    );
    let wayland = common::desktop::start_with(
        home.path(),
        Setup {
            facts: pmt_desktop::platform::Facts {
                os: "linux".into(),
                wayland: true,
                sni_host: Some(true),
            },
            ..Setup::default()
        },
    );
    playwright(
        home.path(),
        "desktop-review.spec.ts",
        &[
            ("PMT_DESKTOP_HARNESS", harness.url.as_str()),
            ("PMT_DESKTOP_WAYLAND", wayland.url.as_str()),
            ("PMT_REPO", repo.to_str().unwrap()),
        ],
    );
    // The turn's commit stays; the reverted new file is gone.
    assert!(repo.join("one.txt").is_file());
    assert!(!repo.join("two.txt").exists());
    let sessions: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("graph-sessions.json")).unwrap(),
    )
    .unwrap();
    let before = prompt_of(home.path(), sessions["before"].as_str().unwrap());
    let after = prompt_of(home.path(), sessions["after"].as_str().unwrap());
    assert!(
        before.contains("saffron"),
        "the open chat keeps its snapshot"
    );
    assert!(
        !after.contains("saffron"),
        "new sessions do not have the memory"
    );
}
