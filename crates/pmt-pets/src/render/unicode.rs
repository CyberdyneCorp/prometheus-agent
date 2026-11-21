//! Truecolor half-blocks: each character cell shows two pixels, the top
//! as the foreground of `▀` and the bottom as its background; a cell
//! whose two pixels are transparent stays a plain space.

use std::fmt::Write as _;

use image::RgbaImage;
use image::imageops::{self, FilterType};

/// One character cell: the top and bottom pixel (RGBA).
pub type Cell = ([u8; 4], [u8; 4]);

const OPAQUE: u8 = 32;

/// Whether a pixel shows.
#[must_use]
pub fn shows(pixel: [u8; 4]) -> bool {
    pixel[3] >= OPAQUE
}

/// `frame` as rows of cells, `cols` wide, keeping its aspect (two pixel
/// rows a cell).
#[must_use]
pub fn cells(frame: &RgbaImage, cols: u32) -> Vec<Vec<Cell>> {
    let cols = cols.max(4);
    let (w, h) = frame.dimensions();
    let height = u64::from(cols) * u64::from(h) / u64::from(w.max(1));
    let rows = u32::try_from(height.div_ceil(2).max(1)).unwrap_or(1);
    let small = imageops::resize(frame, cols, rows * 2, FilterType::Triangle);
    (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| (small.get_pixel(c, r * 2).0, small.get_pixel(c, r * 2 + 1).0))
                .collect()
        })
        .collect()
}

/// The ANSI text of `cells`, one string per row, each ending in a reset.
#[must_use]
pub fn lines(cells: &[Vec<Cell>]) -> Vec<String> {
    cells
        .iter()
        .map(|row| {
            let mut line = String::new();
            for &(top, bottom) in row {
                match (shows(top), shows(bottom)) {
                    (false, false) => line.push_str("\x1b[0m "),
                    (true, false) => {
                        let _ = write!(line, "\x1b[0m\x1b[38;2;{};{};{}m▀", top[0], top[1], top[2]);
                    }
                    (false, true) => {
                        let _ = write!(
                            line,
                            "\x1b[0m\x1b[38;2;{};{};{}m▄",
                            bottom[0], bottom[1], bottom[2]
                        );
                    }
                    (true, true) => {
                        let _ = write!(
                            line,
                            "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
                            top[0], top[1], top[2], bottom[0], bottom[1], bottom[2]
                        );
                    }
                }
            }
            line.push_str("\x1b[0m");
            line
        })
        .collect()
}

/// The shape of `cells` without colour (snapshots): `█` both pixels,
/// `▀` top only, `▄` bottom only, space neither.
#[must_use]
pub fn shape(cells: &[Vec<Cell>]) -> String {
    cells
        .iter()
        .map(|row| {
            row.iter()
                .map(|&(t, b)| match (shows(t), shows(b)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
