//! Terminal pet rendering (spec: pets, Terminal pet rendering): the
//! effective mode ([`detect`]), each protocol's encoder and a
//! [`Renderer`] that turns an atlas into placed, encoded frames.
//!
//! Graphics escapes are only ever produced for a terminal: [`resolve`]
//! answers [`Mode::Off`] when the output is not a TTY, whatever the
//! configuration says, and every surface asks it before drawing.

pub mod detect;
mod iterm;
mod kitty;
mod sixel;
pub mod unicode;

use std::collections::HashMap;
use std::fmt::Write as _;

use image::RgbaImage;

use crate::atlas::Atlas;
use crate::frames;
use crate::size;
use crate::state::PetState;

pub use detect::{TerminalEnv, detect, resolve};

/// How a terminal draws the pet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The kitty graphics protocol.
    Kitty,
    /// iTerm2 inline images.
    Iterm,
    /// DEC sixel.
    Sixel,
    /// Truecolor half-blocks.
    Unicode,
    /// Not drawn.
    Off,
}

impl Mode {
    /// Its name (`pets doctor`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Iterm => "iterm",
            Self::Sixel => "sixel",
            Self::Unicode => "unicode",
            Self::Off => "off",
        }
    }

    /// Whether the mode draws pixels (not text cells).
    #[must_use]
    pub const fn graphics(self) -> bool {
        matches!(self, Self::Kitty | Self::Iterm | Self::Sixel)
    }
}

/// Nominal terminal cell size in pixels, for sizing graphics.
pub const CELL_PX: (u32, u32) = (8, 16);

/// One encoded frame and the cell box it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sprite {
    /// Columns covered.
    pub cols: u16,
    /// Rows covered.
    pub rows: u16,
    /// The escape sequence (graphics) or the lines (half-blocks), each
    /// line without a newline.
    pub body: Body,
}

/// What a sprite is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// One graphics escape drawn at the top-left cell.
    Graphics(String),
    /// Half-block lines, one per row.
    Lines(Vec<String>),
}

/// Encodes an atlas's frames for one mode and size.
#[derive(Debug)]
pub struct Renderer {
    mode: Mode,
    image_id: u32,
    cols: u32,
    frames: HashMap<PetState, Vec<RgbaImage>>,
}

impl Renderer {
    /// A renderer of `atlas` (the pet `slug`) in `mode` at `scale`;
    /// half-blocks take `unicode_cols` columns (0: from `scale`).
    #[must_use]
    pub fn new(atlas: &Atlas, slug: &str, mode: Mode, scale: f64, unicode_cols: u32) -> Self {
        let (width, height) = size::scaled_cell(scale);
        let frames = PetState::ALL
            .into_iter()
            .map(|state| {
                let native = frames::frames(atlas, state);
                let sized = if mode.graphics() {
                    fit_to_cells(&frames::trimmed(&frames::scaled(&native, width, height)))
                } else {
                    frames::trimmed(&native)
                };
                (state, sized)
            })
            .collect();
        Self {
            mode,
            image_id: kitty::image_id(slug),
            cols: size::unicode_cols(scale, unicode_cols),
            frames,
        }
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Frames of `state`.
    #[must_use]
    pub fn frame_count(&self, state: PetState) -> usize {
        self.frames.get(&state).map_or(0, Vec::len)
    }

    /// Frame `index` (modulo the count) of `state`; `None` when off or
    /// the state has no frames.
    #[must_use]
    pub fn sprite(&self, state: PetState, index: usize) -> Option<Sprite> {
        if self.mode == Mode::Off {
            return None;
        }
        let frames = self.frames.get(&state).filter(|f| !f.is_empty())?;
        let frame = &frames[index % frames.len()];
        let (cols, rows) = cell_box(frame);
        let body = match self.mode {
            Mode::Kitty => Body::Graphics(kitty::encode(frame, self.image_id, cols, rows)),
            Mode::Iterm => Body::Graphics(iterm::encode(frame, cols, rows)),
            Mode::Sixel => Body::Graphics(sixel::encode(frame)),
            Mode::Unicode => {
                let lines = unicode::lines(&unicode::cells(frame, self.cols));
                let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
                return Some(Sprite {
                    cols: u16::try_from(self.cols).unwrap_or(u16::MAX),
                    rows,
                    body: Body::Lines(lines),
                });
            }
            Mode::Off => return None,
        };
        Some(Sprite { cols, rows, body })
    }

    /// Half-block cells of frame `index` of `state` (the TUI paints them
    /// into its own buffer).
    #[must_use]
    pub fn cells(&self, state: PetState, index: usize) -> Vec<Vec<unicode::Cell>> {
        let Some(frames) = self.frames.get(&state).filter(|f| !f.is_empty()) else {
            return Vec::new();
        };
        unicode::cells(&frames[index % frames.len()], self.cols)
    }

    /// What removes the sprite (a kitty image is deleted; text cells are
    /// overwritten by [`clear_at`]).
    #[must_use]
    pub fn delete(&self) -> String {
        match self.mode {
            Mode::Kitty => kitty::delete(self.image_id),
            _ => String::new(),
        }
    }
}

/// `frames` (non-empty) resized so each side is a whole number of cells:
/// a terminal fits an image to its cell box, and a partial cell clips.
fn fit_to_cells(frames: &[RgbaImage]) -> Vec<RgbaImage> {
    let Some(first) = frames.first() else {
        return Vec::new();
    };
    let (w, h) = first.dimensions();
    let round = |n: u32, cell: u32| (n.div_ceil(cell)).max(1) * cell;
    frames::scaled(frames, round(w, CELL_PX.0), round(h, CELL_PX.1))
}

fn cell_box(frame: &RgbaImage) -> (u16, u16) {
    let (w, h) = frame.dimensions();
    let cols = (w / CELL_PX.0).max(1);
    let rows = (h / CELL_PX.1).max(1);
    (
        u16::try_from(cols).unwrap_or(u16::MAX),
        u16::try_from(rows).unwrap_or(u16::MAX),
    )
}

/// `sprite` drawn with its top-left cell at 1-based (`row`, `col`), the
/// cursor saved before and restored after.
#[must_use]
pub fn draw_at(sprite: &Sprite, row: u16, col: u16) -> String {
    let mut out = String::from("\x1b7");
    match &sprite.body {
        Body::Graphics(escape) => {
            let _ = write!(out, "\x1b[{row};{col}H{escape}");
        }
        Body::Lines(lines) => {
            for (offset, line) in (0_u16..).zip(lines) {
                let _ = write!(out, "\x1b[{};{col}H{line}", row + offset);
            }
        }
    }
    out.push_str("\x1b8");
    out
}

/// Blanks the `cols` x `rows` box at 1-based (`row`, `col`).
#[must_use]
pub fn clear_at(row: u16, col: u16, cols: u16, rows: u16) -> String {
    let mut out = String::from("\x1b7\x1b[0m");
    let blank = " ".repeat(usize::from(cols));
    for offset in 0..rows {
        let _ = write!(out, "\x1b[{};{col}H{blank}", row + offset);
    }
    out.push_str("\x1b8");
    out
}

/// Whether `text` holds a graphics escape (kitty APC, iTerm2 OSC 1337 or
/// a sixel DCS): what must never reach a pipe or file.
#[must_use]
pub fn has_graphics(text: &str) -> bool {
    text.contains("\x1b_G") || text.contains("\x1b]1337;File=") || text.contains("\x1bP0;1;0q")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::fixture::atlas;

    #[test]
    fn graphics_modes_cover_whole_cells_and_off_draws_nothing() {
        let atlas = atlas([6, 0, 0, 4, 2, 3, 5, 8, 1]);
        for mode in [Mode::Kitty, Mode::Iterm, Mode::Sixel] {
            let renderer = Renderer::new(&atlas, "boba", mode, 0.5, 0);
            let sprite = renderer.sprite(PetState::Idle, 0).unwrap();
            assert!(sprite.cols > 0 && sprite.rows > 0);
            let Body::Graphics(escape) = &sprite.body else {
                panic!("{mode:?} is graphics")
            };
            assert!(has_graphics(escape), "{mode:?}");
        }
        let off = Renderer::new(&atlas, "boba", Mode::Off, 0.5, 0);
        assert!(off.sprite(PetState::Idle, 0).is_none());
    }

    #[test]
    fn half_blocks_have_no_graphics_escapes() {
        let atlas = atlas([6, 0, 0, 4, 2, 3, 5, 8, 1]);
        let renderer = Renderer::new(&atlas, "boba", Mode::Unicode, 0.33, 0);
        let sprite = renderer.sprite(PetState::Run, 3).unwrap();
        assert_eq!(sprite.cols, 16);
        let text = draw_at(&sprite, 1, 60);
        assert!(!has_graphics(&text));
        assert!(text.starts_with("\x1b7") && text.ends_with("\x1b8"));
    }
}
