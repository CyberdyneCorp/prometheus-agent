//! The pet in the TUI on a pseudo-terminal (spec: pets, Pet slash
//! command; Terminal pet rendering; Affection reactions; Achievement
//! unlocks celebrate).

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::pets::install;
use common::tui::{enter, opened, see, tui};
use common::{daemon, mock_home, ok, run, turns_started};
use expectrl::Expect;

fn get(home: &std::path::Path, key: &str) -> String {
    ok(&run(home, &["config", "get", key]))
        .trim()
        .trim_matches('"')
        .to_owned()
}

/// Scenario: Adopt from the TUI.
#[test]
fn pet_slug_adopts_an_installed_pet_without_a_turn() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba", "mochi"]);
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    enter(&mut p, "/pet mochi");
    see(&mut p, "pet 'mochi' is out");
    assert_eq!(get(home.path(), "pets.slug"), "mochi");
    assert_eq!(get(home.path(), "pets.enabled"), "true");
    // The half-block pet is drawn (TERM=xterm-256color: no graphics
    // protocol), and no turn ran.
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p.expect("▀").unwrap();
    enter(&mut p, "/pet off");
    see(&mut p, "pet hidden");
    assert_eq!(get(home.path(), "pets.enabled"), "false");
    enter(&mut p, "/pet nobody");
    see(&mut p, "no pet 'nobody' is installed");
    assert_eq!(turns_started(home.path()), 0);
}

/// Scenario: Thank-you message (the heart plays beside the pet).
#[test]
fn a_thank_you_plays_the_heart() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    ok(&run(home.path(), &["pets", "select", "boba"]));
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    enter(&mut p, "thanks, good bot");
    p.expect("♥").unwrap();
}

/// Scenario: Unlock during a session (the TUI's toast).
#[test]
fn an_unlock_shows_a_toast() {
    let home = mock_home();
    let zone = night_zone();
    ok(&run(
        home.path(),
        &["config", "set", "cron.timezone", &zone],
    ));
    {
        let home = pmt_config::Home::open(home.path()).unwrap();
        let profile = home
            .profile(&pmt_config::ProfileName::parse("default").unwrap())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            profile,
            &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        let store = pmt_store::Store::open(&scope).unwrap();
        for _ in 0..9 {
            store
                .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
                .unwrap();
        }
    }
    ok(&run(home.path(), &["achievements"]));
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    let deadline = Instant::now() + Duration::from_secs(5);
    while turns_started(home.path()) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    enter(&mut p, "hello");
    see(&mut p, "Achievement unlocked");
    see(&mut p, "Night Owl · silver");
}

fn night_zone() -> String {
    let hour = i32::from(
        jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .hour(),
    );
    let mut offset = (2 - hour).rem_euclid(24);
    if offset > 12 {
        offset -= 24;
    }
    match offset {
        0 => "Etc/GMT".to_owned(),
        east if east > 0 => format!("Etc/GMT-{east}"),
        west => format!("Etc/GMT+{}", -west),
    }
}
