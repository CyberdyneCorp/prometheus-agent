//! The TUI's client-local display commands, busy indicator, command
//! palette and startup banner, on a pseudo-terminal (spec: tui,
//! Client-local display commands; Busy indicator styles; Fuzzy command
//! palette; Collapsible startup banner), and the same display commands in
//! the line interface.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::tui::{enter, finish, opened, see, see_repainted, tui};
use common::{daemon, mock_home};
use expectrl::Expect;

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// Scenario: Hiding the status bar.
#[test]
fn sb_hides_the_status_bar_and_saves_ui_status_bar_false() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    see_repainted(&mut p, "default │ mock/mock");
    enter(&mut p, "/sb");
    see(&mut p, "status bar hidden (ui.status_bar = false)");
    enter(&mut p, "/quit");
    let (_, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(
        config(home.path()).contains("status_bar = false"),
        "{}",
        config(home.path())
    );
    // The next launch starts without it.
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "/statusbar");
    see(&mut p, "status bar shown (ui.status_bar = true)");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// Scenario: Battery on a desktop tower.
#[test]
fn battery_on_without_a_battery_is_saved_and_shows_nothing() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[("PMT_TEST_BATTERY", "none")]);
    opened(&mut p);
    enter(&mut p, "/battery on");
    let mut shown = see(&mut p, "no battery detected, so none is shown");
    enter(&mut p, "/battery status");
    shown += &see(&mut p, "no battery detected");
    shown += &see_repainted(&mut p, "default │ mock/mock");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!(shown + &text).contains('▮'), "no battery item is drawn");
    assert!(config(home.path()).contains("status_bar_battery = true"));
}

#[test]
fn a_battery_leads_the_status_line_when_shown() {
    let home = mock_home();
    common::append_config(home.path(), "\n[ui]\nstatus_bar_battery = true\n");
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[("PMT_TEST_BATTERY", "64,charging")]);
    opened(&mut p);
    see_repainted(&mut p, "▮ 64%+ │ default");
    enter(&mut p, "/battery status");
    see(&mut p, "battery: 64% (charging)");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// Scenario: Switching style.
#[test]
fn indicator_ascii_is_saved_and_the_next_busy_period_uses_it() {
    let home = mock_home();
    let script = home.path().join("slow.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\nlatency_ms = 3000\ntext = \"slow done\"\n",
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script.to_string_lossy().into_owned();
    let mut p = tui(home.path(), &["chat", "--mock-script", &script], &[]);
    opened(&mut p);
    enter(&mut p, "/indicator dots");
    see(
        &mut p,
        "unknown indicator 'dots' (kaomoji, emoji, unicode, ascii)",
    );
    enter(&mut p, "/indicator ascii");
    see(&mut p, "busy indicator: ascii");
    enter(&mut p, "go");
    // The ASCII bar is the only `|` the screen draws.
    p.expect("|").unwrap();
    see(&mut p, "slow done");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!text.contains("(•_•)"), "no kaomoji after the switch");
    assert!(config(home.path()).contains("busy_indicator = \"ascii\""));
}

/// Scenario: Insert, not run.
#[test]
fn the_palette_inserts_the_chosen_command_without_running_it() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    // Ctrl+P, then the query.
    p.send("\u{10}").unwrap();
    see(&mut p, "palette ›");
    p.send("cmpr").unwrap();
    see(&mut p, "/compress");
    p.send("\r").unwrap();
    // The composer now holds it (the palette's own rows were read above).
    see(&mut p, "/compress");
    std::thread::sleep(Duration::from_millis(500));
    // Ctrl+U clears it; nothing ran meanwhile.
    p.send("\u{15}").unwrap();
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!text.contains("compress: "), "/compress never ran");
    // Esc leaves the composer as it was.
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    p.send("keep").unwrap();
    p.send("\u{10}").unwrap();
    see(&mut p, "palette ›");
    p.send("mod\u{1b}").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    enter(&mut p, " this");
    see(&mut p, "mock: keep this");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// Scenario: Many skills installed.
#[test]
fn the_banner_keeps_skills_collapsed_until_enter_opens_them() {
    let home = mock_home();
    for n in 0..80 {
        let dir = home
            .path()
            .join(format!("profiles/default/skills/skill-{n:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: skill-{n:02}\ndescription: Skill {n}.\n---\n\nBody.\n"),
        )
        .unwrap();
    }
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    see(&mut p, "▾ Tools (");
    // Eighty installed (the bundled skills come on top).
    let shown = see(&mut p, "▸ Skills (8");
    assert!(!shown.contains("skill-07"), "collapsed");
    let shown = see(&mut p, "▸ MCP Servers (0)");
    assert!(!shown.contains("skill-07"), "collapsed");
    // Ctrl+B moves to the headers, Down to Skills, Enter opens it.
    p.send("\u{2}").unwrap();
    p.send("\u{1b}[B").unwrap();
    p.send("\r").unwrap();
    see(&mut p, "skill-07");
    // A full repaint shows the open header whole.
    p.send("\u{1b}").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    enter(&mut p, "/redraw");
    see(&mut p, "▾ Skills (8");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    // The next launch starts collapsed again.
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    see(&mut p, "▸ Skills (8");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

#[test]
fn a_click_on_a_banner_header_toggles_it() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    see(&mut p, "▾ Tools (");
    // Row 2 is the Tools header (row 1 is the title); SGR mouse press.
    p.send("\u{1b}[<0;3;2M\u{1b}[<0;3;2m").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    enter(&mut p, "/redraw");
    see(&mut p, "▸ Tools (");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

#[test]
fn redraw_clears_and_repaints_the_screen() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    // Once the model is known, a repaint shows the status line whole.
    see_repainted(&mut p, "default │ mock/mock");
    enter(&mut p, "/redraw");
    p.expect("\u{1b}[2J").unwrap();
    // Everything is painted again, the banner's title included.
    see(&mut p, "Prometheus");
    see(&mut p, "default │ mock/mock");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// The line interface handles the same commands itself: `/battery status`
/// and `/sb` (which turns off its bottom status line).
#[test]
fn the_line_interface_runs_battery_and_statusbar_itself() {
    let home = mock_home();
    let mut p = common::pty::spawn_with(home.path(), &[], &[("PMT_TEST_BATTERY", "none")]);
    p.get_process_mut().set_window_size(100, 30).unwrap();
    // The bottom status line, out of the scrolling region, then the prompt.
    p.expect("\u{1b}[1;29r").unwrap();
    p.expect("default │ mock/mock").unwrap();
    p.expect("› ").unwrap();
    p.send("/battery status\r").unwrap();
    p.expect("no battery detected").unwrap();
    p.send("/sb\r").unwrap();
    p.expect("status bar hidden (ui.status_bar = false)")
        .unwrap();
    // The row is given back: the scrolling region is reset.
    p.expect("\u{1b}[r").unwrap();
    p.send("/redraw\r").unwrap();
    p.expect("\u{1b}[2J").unwrap();
    p.send("/exit\r").unwrap();
    p.expect(expectrl::Eof).unwrap();
    assert!(config(home.path()).contains("status_bar = false"));
}
