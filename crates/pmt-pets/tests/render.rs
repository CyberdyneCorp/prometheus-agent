//! Terminal pet rendering snapshots (spec: pets, Terminal pet rendering):
//! a fixture pet drawn as half-blocks for each state, the head of each
//! graphics protocol's escape, and the cell box at several scales.
//! `PMT_UPDATE_SNAPSHOTS=1` rewrites the files (review the diff).

use std::fmt::Write as _;
use std::path::Path;

use image::{Rgba, RgbaImage};
use pmt_pets::PetState;
use pmt_pets::atlas::{Atlas, FRAME_H, FRAME_W, HEIGHT, WIDTH};
use pmt_pets::render::{Body, Mode, Renderer, unicode};

fn check(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
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

/// Whether pixel (`x`, `y`) of frame `column` is the round body (the eyes,
/// which move with the frame, are holes).
fn body(x: u32, y: u32, column: u32) -> bool {
    let (dx, dy) = (f64::from(x) - 96.0, f64::from(y) - 120.0);
    let inside = dx * dx / 3600.0 + dy * dy / 4900.0 <= 1.0;
    let eye = y / 8 == 12 && (x / 8 == 9 + column % 3 || x / 8 == 13 + column % 3);
    inside && !eye
}

/// One frame drawn into `image` at row `row`, column `column`.
fn paint(image: &mut RgbaImage, row: u32, column: u32) {
    let (ox, oy) = (column * FRAME_W, row * FRAME_H);
    for y in 0..FRAME_H {
        for x in (0..FRAME_W).filter(|&x| body(x, y, column)) {
            image.put_pixel(ox + x, oy + y, Rgba([240, 140, 60, 255]));
        }
    }
}

/// A round body whose eyes move with the frame; each row holds two to
/// five frames and the waiting row is empty.
fn atlas() -> Atlas {
    let mut image = RgbaImage::new(WIDTH, HEIGHT);
    for row in 0..9_u32 {
        let frames = if row == 6 { 0 } else { row % 4 + 2 };
        for column in 0..frames {
            paint(&mut image, row, column);
        }
    }
    Atlas::from_image(image).unwrap().0
}

#[test]
fn half_block_frames_of_each_state() {
    let atlas = atlas();
    let renderer = Renderer::new(&atlas, "fixture", Mode::Unicode, 0.33, 24);
    let mut text = String::new();
    for state in PetState::ALL {
        let _ = writeln!(
            text,
            "== {} ({} frames)",
            state.as_str(),
            renderer.frame_count(state)
        );
        let _ = writeln!(text, "{}", unicode::shape(&renderer.cells(state, 1)));
    }
    check("half-blocks", &text);
}

#[test]
fn graphics_escapes_and_cell_boxes() {
    let atlas = atlas();
    let mut text = String::new();
    for mode in [Mode::Kitty, Mode::Iterm, Mode::Sixel] {
        for scale in [0.33, 1.0] {
            let renderer = Renderer::new(&atlas, "fixture", mode, scale, 0);
            let sprite = renderer.sprite(PetState::Idle, 0).unwrap();
            let Body::Graphics(escape) = &sprite.body else {
                panic!("{mode:?}")
            };
            let head: String = escape
                .chars()
                .take(48)
                .collect::<String>()
                .replace('\x1b', "ESC");
            let _ = writeln!(
                text,
                "{} @{scale}: {}x{} cells, {head}",
                mode.as_str(),
                sprite.cols,
                sprite.rows
            );
        }
    }
    let kitty = Renderer::new(&atlas, "fixture", Mode::Kitty, 0.33, 0);
    let _ = writeln!(
        text,
        "kitty delete: {}",
        kitty.delete().replace('\x1b', "ESC")
    );
    check("graphics", &text);
}

#[test]
fn half_block_lines_are_truecolor_and_reset() {
    let atlas = atlas();
    let renderer = Renderer::new(&atlas, "fixture", Mode::Unicode, 0.33, 0);
    let sprite = renderer.sprite(PetState::Wave, 0).unwrap();
    let Body::Lines(lines) = &sprite.body else {
        panic!()
    };
    assert_eq!(usize::from(sprite.rows), lines.len());
    assert!(lines.iter().all(|l| l.ends_with("\x1b[0m")));
    assert!(lines.iter().any(|l| l.contains("\x1b[38;2;240;140;60m")));
    assert!(!pmt_pets::render::has_graphics(&lines.concat()));
}
