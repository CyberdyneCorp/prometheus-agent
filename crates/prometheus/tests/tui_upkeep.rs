//! What the TUI, a client of the daemon, shows as sessions end and open
//! (spec: memory, Reviewable memory changes per session; session-storage,
//! Low-disk prune prompt): the memory review summary when the TUI leaves
//! a session and when it resumes one, and the low-disk prune question at
//! startup, answered on its card and applied by the daemon.

#![cfg(all(unix, feature = "gateway", feature = "tui"))]

mod common;

use std::path::Path;
use std::time::Duration;

use common::tui::{LEAVE, enter, finish, opened, see, see_repainted, tui};
use common::{daemon, mock_home};

const SUMMARY: &str = "Memory: +2 ~1 -0 (review with /memory review)";

/// Two adds then a patch through the memory tool, then an answer.
fn memory_script(home: &Path) -> String {
    let path = home.join("memory.toml");
    std::fs::write(
        &path,
        r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Lives in Lisbon." }, { action = "add", content = "Works in UTC." }] }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "patch", old_text = "Lisbon", content = "Lives in Porto." }] }

[[rule]]
step = 2
text = "noted"
"#,
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Scenario: Session end shows a summary (on the TUI, a daemon client),
/// and again when the session is resumed.
#[test]
fn the_tui_shows_the_memory_summary_when_a_session_ends_and_when_it_is_resumed() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = memory_script(home.path());
    let mut p = tui(home.path(), &["chat", "--mock-script", &script], &[]);
    opened(&mut p);
    enter(&mut p, "remember where I live");
    see(&mut p, "noted");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    let after = text.rsplit(LEAVE).next().unwrap();
    assert!(
        after.contains(SUMMARY),
        "printed as the TUI leaves: {after:?}"
    );

    let session = common::newest_session(home.path());
    let mut p = tui(home.path(), &["chat", "-r", &session], &[]);
    opened(&mut p);
    see(&mut p, SUMMARY);
    // Reviewed changes are not summarized again.
    enter(&mut p, "/memory review keep all");
    see(&mut p, "Kept 3 change(s).");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(
        !text.rsplit(LEAVE).next().unwrap().contains("Memory: +"),
        "{text:?}"
    );
}

/// The low-disk prune question at a TUI startup: `on` turns auto-prune on
/// through the daemon, and a second startup the same day does not ask.
#[test]
fn the_tui_offers_auto_prune_at_startup_when_disk_space_is_low() {
    let home = mock_home();
    // Every volume has less free space than this.
    common::append_config(
        home.path(),
        "\n[sessions]\nlow_disk_bytes = 9223372036854775807\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    see(&mut p, "Disk space is low.");
    // Only the cells that change are written: the card shares words with
    // the question above it, so it is checked on a full repaint.
    see_repainted(&mut p, "type on, off (never ask again) or later");
    enter(&mut p, "on");
    see_repainted(&mut p, "Auto-prune on: ended sessions");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("7776000"), "{config}");

    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    std::thread::sleep(Duration::from_secs(1));
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!text.contains("Disk space is low"), "asked once: {text:?}");
}
