//! The pet and achievement toasts on the screen (spec: pets, Terminal pet
//! rendering; Achievement unlocks celebrate): half-block cells in the
//! transcript's bottom-right corner with the heart beside them, a
//! graphics pet's reserved box, and the toast in the top-right corner.
//! `PMT_UPDATE_SNAPSHOTS=1` rewrites the snapshots.

use pmt_config::schema::TuiTheme;
use pmt_tui::app::App;
use pmt_tui::pet::{PetView, Toast};
use pmt_tui::status::Status;
use pmt_tui::theme::Theme;
use pmt_tui::transcript::Kind;

mod common;

use common::{check, screen};

fn app() -> App {
    let status = Status {
        profile: "default".into(),
        model: Some("mock/m1".into()),
        ..Status::default()
    };
    let mut app = App::new(Theme::new(TuiTheme::Dark, true), status);
    app.transcript.push(Kind::User, "thanks, good bot");
    app.transcript.push(Kind::Assistant, "You're welcome!");
    app
}

/// A 6x3 cell pet: a round body, transparent corners.
fn cells() -> Vec<Vec<([u8; 4], [u8; 4])>> {
    let (on, off) = ([240, 140, 60, 255], [0, 0, 0, 0]);
    vec![
        vec![
            (off, off),
            (off, on),
            (on, on),
            (on, on),
            (off, on),
            (off, off),
        ],
        vec![(on, on), (on, on), (on, on), (on, on), (on, on), (on, on)],
        vec![
            (off, off),
            (on, off),
            (on, on),
            (on, on),
            (on, off),
            (off, off),
        ],
    ]
}

#[test]
fn the_pet_sits_in_the_transcripts_corner_with_the_heart() {
    let mut app = app();
    app.pet = Some(PetView {
        cells: cells(),
        graphics: None,
        heart: true,
    });
    let shown = screen(&app, 60, 12);
    check("pet-heart-60x12", &shown);
    let area = app.pet_area().expect("drawn");
    assert_eq!((area.width, area.height), (6, 3));
}

#[test]
fn a_graphics_pet_reserves_its_box_and_a_toast_shows_top_right() {
    let mut app = app();
    app.pet = Some(PetView {
        cells: Vec::new(),
        graphics: Some((8, 4)),
        heart: false,
    });
    app.toast = Some(Toast {
        title: "Achievement unlocked".into(),
        body: "Night Owl · silver".into(),
    });
    let shown = screen(&app, 70, 14);
    check("pet-graphics-toast-70x14", &shown);
    let area = app.pet_area().expect("reserved");
    assert_eq!((area.width, area.height), (8, 4));
}

#[test]
fn a_pet_too_big_for_the_screen_is_left_out() {
    let mut app = app();
    app.pet = Some(PetView {
        cells: Vec::new(),
        graphics: Some((40, 20)),
        heart: false,
    });
    screen(&app, 60, 12);
    assert!(app.pet_area().is_none());
}
