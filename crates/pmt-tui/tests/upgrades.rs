//! Screen snapshots of the M3 upgrades (spec: tui, Split panes for
//! subagents and tool output; Inline diff review for file edits; Session
//! timeline scrubber; Shared live sessions): a pane beside the transcript
//! at 130 columns, below it at 100 and as a tab at 70; an approval card
//! with a numbered diff; the timeline over a read-only preview; the
//! status line of a shared session.

use std::time::{Duration, Instant};

use pmt_config::schema::TuiTheme;
use pmt_tui::app::{App, Card, Overlay};
use pmt_tui::diff::DiffHunk;
use pmt_tui::panes::{Call, Child, Streamed, Which};
use pmt_tui::status::Status;
use pmt_tui::theme::Theme;
use pmt_tui::timeline::{Timeline, Turn};
use pmt_tui::transcript::{Kind, Transcript};

mod common;

use common::{check, screen};

fn app() -> App {
    let status = Status {
        profile: "default".into(),
        model: Some("mock/m1".into()),
        ..Status::default()
    };
    let mut app = App::new(Theme::new(TuiTheme::Dark, true), status);
    app.transcript.push(Kind::User, "split the work");
    app.transcript.push(Kind::Tool, "delegate_task");
    app.transcript.push(Kind::Assistant, "dispatched both");
    app
}

/// Two finished children (a running one's elapsed time would move), the
/// second selected with its streamed transcript.
fn with_children(mut app: App) -> App {
    let start = Instant::now();
    let mut first = Child::new("sa-1", "first child job", start);
    first.status = "completed".into();
    first.took = Some(Duration::from_secs(2));
    first.tokens = 120;
    let mut second = Child::new("sa-2", "second child job", start);
    second.status = "completed".into();
    second.took = Some(Duration::from_secs(4));
    second.tokens = 340;
    second.cost_usd = Some(0.0012);
    second.stream = vec![
        Streamed::Call("read_file".into(), "notes.txt".into()),
        Streamed::Output("remember the milk".into()),
        Streamed::Text("second child read the notes".into()),
        Streamed::Ended("completed".into()),
    ];
    app.panes.children = vec![first, second];
    app.panes.child = 1;
    app.panes.toggle(Which::Subagents);
    app
}

/// Scenario: Watching a delegated child (wide: side by side).
#[test]
fn the_subagents_pane_sits_beside_the_transcript_from_120_columns() {
    check(
        "pane-beside-130x14",
        &screen(&with_children(app()), 130, 14),
    );
}

/// Scenario: Watching a delegated child (80-119 columns: stacked).
#[test]
fn the_subagents_pane_sits_below_the_transcript_from_80_to_119_columns() {
    check("pane-below-100x20", &screen(&with_children(app()), 100, 20));
}

/// Under 80 columns the pane is a tab; the tool pane holds a call's whole
/// output.
#[test]
fn under_80_columns_the_tool_pane_is_a_tab_with_the_full_output() {
    let mut app = app();
    app.panes.calls = vec![Call {
        id: "c1".into(),
        tool: "terminal".into(),
        hint: "cargo test".into(),
        output: Some("running 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok".into()),
    }];
    app.panes.toggle(Which::Tools);
    check("pane-tabs-70x12", &screen(&app, 70, 12));
}

/// Scenario: Partial acceptance (the card).
#[test]
fn an_edit_approval_shows_numbered_hunks_and_the_review_choices() {
    let mut app = app();
    let hunk = |at: &str, old: &str, new: &str| DiffHunk {
        header: format!("@@ -{at},3 +{at},3 @@"),
        lines: vec![
            " keep".into(),
            format!("-{old}"),
            format!("+{new}"),
            " keep".into(),
        ],
        syntax: Vec::new(),
    };
    app.card = Some(Card {
        title: "approval needed: write_file AGENTS.md".into(),
        body: "protected-file: an agent instruction file (AGENTS.md)".into(),
        diff: vec![
            hunk("1", "b", "B"),
            hunk("6", "g", "G"),
            hunk("12", "m", "M"),
        ],
        hint: "[y] accept all  [n] reject  hunks 1,3: accept those  [e] edit then accept".into(),
    });
    check("diff-card-90x26", &screen(&app, 90, 26));
}

/// Scenario: Rewind keeps the original (the scrubber).
#[test]
fn the_timeline_lists_turns_over_a_read_only_preview_of_the_highlighted_one() {
    let mut app = app();
    let turns = (1..=12)
        .map(|n| Turn {
            number: n,
            at: format!("10:{n:02}:00"),
            first_line: format!("step {n}"),
            tools: n % 3,
            cost_usd: (n % 2 == 0).then_some(0.001),
        })
        .collect();
    let mut timeline = Timeline::new(turns);
    timeline.move_selection(-5);
    let mut preview = Transcript::default();
    preview.push(Kind::User, "step 7");
    preview.push(Kind::Assistant, "mock: step 7");
    app.preview = Some(("turn 7 (read-only preview)".into(), preview));
    app.overlay = Some(Overlay::Timeline(timeline));
    check("timeline-90x20", &screen(&app, 90, 20));
}

/// Scenario: Desktop joins a TUI session (the status line).
#[test]
fn a_shared_session_names_the_other_clients_in_the_status_line() {
    let mut app = app();
    app.status.shared = Some("with desktop-7".into());
    app.transcript
        .push(Kind::User, "[desktop-7] hello from the desk");
    check("shared-80x8", &screen(&app, 80, 8));
}
