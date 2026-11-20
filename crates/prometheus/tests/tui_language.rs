//! The TUI's language picker on a pseudo-terminal (spec: i18n, Language
//! switchers show endonyms): `/language` lists every selectable language
//! by endonym and id, and choosing one writes `display.language` through
//! the daemon's `config.patch`.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::tui::{enter, opened, see, tui};
use common::{daemon, mock_home, ok, run};
use expectrl::Expect;

#[test]
fn the_picker_lists_endonyms_and_writes_the_choice() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    enter(&mut p, "/language");
    see(&mut p, "language: ↑↓ select");
    see(&mut p, "Deutsch (de)");
    // Sorted by id: af, ar, de, en... English (the current one) is
    // highlighted; two rows up is Arabic.
    p.send("\u{1b}[A").unwrap();
    p.send("\u{1b}[A").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.send("\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let language = ok(&run(home.path(), &["config", "get", "display.language"]));
        if language.trim().trim_matches('"') == "ar" {
            break;
        }
        assert!(Instant::now() < deadline, "display.language is {language}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
