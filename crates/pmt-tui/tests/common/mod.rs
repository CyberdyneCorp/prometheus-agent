//! The screen rendered into a test backend and compared with the text in
//! `tests/snapshots/<name>.txt`. `PMT_UPDATE_SNAPSHOTS=1` rewrites them
//! (review the diff before committing).

use pmt_tui::app::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

pub(crate) fn screen(app: &App, width: u16, height: u16) -> String {
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

pub(crate) fn check(name: &str, actual: &str) {
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
