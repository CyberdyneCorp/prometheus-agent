//! Compaction notices in a messaging chat served by the real daemon
//! (spec: context-compression, Compaction progress notices and context
//! pressure gauge; Idle compaction): an automatic compaction is silent in
//! the chat by default and announced with `compression.progress_notices`.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};

/// `q1`..`q4` get ~1,500-token answers; anything else (the summary
/// request too) gets `fast done`.
fn rules() -> String {
    format!(
        "[[rule]]\nuser_regex = '^q[0-9]$'\ntext = \"{}\"\nrepeat = true\n\n\
         [[rule]]\nrepeat = true\ntext = \"fast done\"\n",
        "long notes ".repeat(550),
    )
}

const CONFIG: &str = "\n[models.\"mock/mock\"]\ncontext_window = 16000\n\n\
                      [compression]\nidle_compact_after_secs = 1\n";

/// The `compression.committed` lines of the default profile's live
/// trajectories.
fn compactions(home: &Path) -> Vec<String> {
    std::fs::read_dir(home.join("profiles/default/trajectories"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|text| {
            text.lines()
                .filter(|l| l.contains("\"compression.committed\""))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Four long exchanges, a pause, then a message that compacts first
/// (trigger `idle`).
fn idle_compaction(home: &Path) {
    let long = |home: &Path| {
        texts(home, "default")
            .iter()
            .filter(|t| t.contains("long notes"))
            .count()
    };
    for (n, q) in ["q1", "q2", "q3", "q4"].iter().enumerate() {
        send(home, "default", "u1", q);
        wait("the answer", Duration::from_mins(1), || long(home) == n + 1);
    }
    std::thread::sleep(Duration::from_millis(1_200));
    send(home, "default", "u1", "back again");
    wait_said(home, "default", "fast done");
    let committed = compactions(home);
    assert!(
        committed.iter().any(|c| c.contains("\"idle\"")),
        "{committed:?}"
    );
}

/// Scenario: Silent on chat by default.
#[test]
fn an_automatic_compaction_posts_nothing_to_the_chat_by_default() {
    let home = gateway_home_with(&rules(), CONFIG);
    let home = home.path();
    let daemon = Daemon::start(home);
    idle_compaction(home);
    let said = texts(home, "default");
    assert!(
        !said.iter().any(|t| t.contains("ompact")),
        "no compaction status in the chat: {said:?}"
    );
    daemon.stop(home);
}

#[test]
fn progress_notices_announce_the_idle_compaction() {
    let home = gateway_home_with(&rules(), &format!("{CONFIG}progress_notices = true\n"));
    let home = home.path();
    let daemon = Daemon::start(home);
    idle_compaction(home);
    wait_said(home, "default", "Compacting the context after a long pause");
    wait_said(home, "default", "Context compacted (idle)");
    daemon.stop(home);
}
