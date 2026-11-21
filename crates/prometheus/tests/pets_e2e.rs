//! Pets and achievements in the browser (spec: pets, Desktop pet and
//! pop-out overlay; Achievements from session history; Achievement
//! unlocks celebrate): the desktop page in Chromium (`common/desktop.rs`)
//! and the dashboard's `/achievements` page.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::path::Path;

use common::dashboard::playwright;
use common::pets::{Gallery, atlas};
use common::{mock_home, ok, run};

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

fn scope(home: &Path) -> pmt_config::ProfileScope {
    let home = pmt_config::Home::open(home).unwrap();
    let profile = home
        .profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    pmt_config::ProfileScope::open(
        profile,
        &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
        pmt_config::Launch::Other,
    )
    .unwrap()
}

fn sessions(home: &Path, n: usize) {
    let store = pmt_store::Store::open(&scope(home)).unwrap();
    for _ in 0..n {
        store
            .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
            .unwrap();
    }
}

fn desktop(home: &Path, spec: &str) {
    let _stop = Stop(home);
    let harness = common::desktop::start(home);
    playwright(home, spec, &[("PMT_DESKTOP_HARNESS", harness.url.as_str())]);
}

/// An `Etc/GMT` zone in which it is now between 02:00 and 02:59.
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

fn pets(home: &Path) -> Gallery {
    let gallery = Gallery::start(
        home,
        &[("boba", atlas(6)), ("mochi", atlas(4)), ("taro", atlas(5))],
    );
    for slug in ["boba", "mochi"] {
        ok(&run(home, &["pets", "install", slug]));
    }
    ok(&run(home, &["pets", "select", "boba"]));
    gallery
}

/// Scenario: Prompt from the overlay. The floating pet resizes with Alt +
/// wheel (persisted as `pets.scale`), pops out with Shift + click into the
/// overlay, which has no connection: a prompt typed on it goes to the most
/// recent session on the main window's connection without restoring the
/// app; a turn finished meanwhile shows the mail badge.
#[test]
fn the_desktop_pet_pops_out_and_prompts_through_the_main_window() {
    let home = mock_home();
    let _gallery = pets(home.path());
    desktop(home.path(), "desktop-pet.spec.ts");
    let scale = ok(&run(home.path(), &["config", "get", "pets.scale"]));
    assert_eq!(scale.trim(), "0.43");
}

/// `/pet off`, `/pet <slug>`, `/pet list` (the palette's pet list, which
/// also installs a gallery pet) and `/hatch` without a reference-capable
/// backend run in the window; none starts a turn. A thank-you plays the
/// heart and is sent as typed.
#[test]
fn desktop_pet_commands_run_in_the_window() {
    let home = mock_home();
    let _gallery = pets(home.path());
    desktop(home.path(), "desktop-pet-commands.spec.ts");
    assert!(
        home.path()
            .join("profiles/default/pets/taro/pet.toml")
            .is_file()
    );
    let slug = ok(&run(home.path(), &["config", "get", "pets.slug"]));
    assert_eq!(slug.trim().trim_matches('"'), "taro");
}

/// Scenario: Unlock during a session (desktop): the toast and the jump.
#[test]
fn a_desktop_unlock_shows_a_toast_and_the_pet_jumps() {
    let home = mock_home();
    let _gallery = pets(home.path());
    ok(&run(
        home.path(),
        &["config", "set", "cron.timezone", &night_zone()],
    ));
    sessions(home.path(), 9);
    ok(&run(home.path(), &["achievements"]));
    desktop(home.path(), "desktop-pet-unlock.spec.ts");
}

/// Scenario: First scan does not block (dashboard).
#[test]
fn the_achievements_page_renders_before_the_first_scan_ends() {
    let home = mock_home();
    sessions(home.path(), 400);
    playwright(home.path(), "achievements.spec.ts", &[]);
    assert!(
        home.path()
            .join("profiles/default/achievements.json")
            .is_file()
    );
}
