//! Screen snapshots (spec: tui, Status line and accessibility; Slash
//! command flow; Configurable key bindings; Composer and path completion):
//! the screen rendered into a test backend and compared with the text in
//! `tests/snapshots/<name>.txt`. `PMT_UPDATE_SNAPSHOTS=1` rewrites them
//! (review the diff before committing).

use pmt_config::schema::TuiTheme;
use pmt_tui::app::{App, Card};
use pmt_tui::keys::Bindings;
use pmt_tui::status::Status;
use pmt_tui::theme::Theme;
use pmt_tui::transcript::Kind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn screen(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..height {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buffer[(x, y)].symbol());
        }
        text.push_str(row.trim_end());
        text.push('\n');
    }
    text
}

fn check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("PMT_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (PMT_UPDATE_SNAPSHOTS=1 writes it)", path.display()));
    assert_eq!(actual, expected, "snapshot {name} differs");
}

fn app() -> App {
    let status = Status {
        profile: "default".into(),
        model: Some("mock/m1".into()),
        context_tokens: Some(2048),
        context_window: Some(8192),
        cost_usd: Some(0.0042),
        ..Status::default()
    };
    let mut app = App::new(Theme::new(TuiTheme::Dark, true), status);
    app.transcript.push(Kind::User, "summarise the build log");
    app.transcript.push(Kind::Tool, "read_file");
    app.transcript.push(
        Kind::Assistant,
        "The build failed in the linker step: two crates define the same symbol.",
    );
    app
}

#[test]
fn the_screen_shows_transcript_composer_and_status() {
    let mut app = app();
    app.composer.set("and how do I fix it?");
    check("conversation-80x14", &screen(&app, 80, 14));
}

#[test]
fn a_40_by_12_terminal_keeps_the_composer_and_the_last_line() {
    let mut app = app();
    for n in 0..30 {
        app.transcript.push(Kind::Notice, format!("notice {n}"));
    }
    app.transcript
        .push(Kind::Assistant, "the newest line of the transcript");
    app.composer
        .set("a prompt long enough to scroll past forty columns");
    let shown = screen(&app, 40, 12);
    assert!(shown.contains("the newest line of the"), "{shown}");
    assert!(shown.contains("› "), "{shown}");
    check("narrow-40x12", &shown);
    // Smaller still: nothing panics.
    for (w, h) in [(10, 3), (1, 1), (40, 4)] {
        screen(&app, w, h);
    }
}

#[test]
fn an_approval_card_sits_above_the_composer_and_counts_in_the_status() {
    let mut app = app();
    app.status.approvals = 1;
    app.status.busy = true;
    app.status.reduced_motion = true;
    app.card = Some(Card {
        title: "approval needed: terminal rm -rf build".into(),
        body: "dangerous-pattern: recursive delete (rm -rf)".into(),
        hint: "[o]nce  [s]ession  [a]lways  [d]eny   (answer within 300 s)".into(),
    });
    check("approval-80x14", &screen(&app, 80, 14));
}

#[test]
fn slash_completions_list_above_the_composer() {
    let mut app = app();
    app.composer.set("/comp");
    app.completions = vec![
        "/compact  compact the context now".into(),
        "/compress  summarise older turns".into(),
    ];
    check("completions-60x12", &screen(&app, 60, 12));
}

#[test]
fn keys_lists_the_effective_bindings() {
    let mut app = app();
    app.transcript
        .push(Kind::Notice, Bindings::default().describe());
    check("keys-60x24", &screen(&app, 60, 24));
}
