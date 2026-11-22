//! The animation frames of one state (spec: pets, Pet storage and sprite
//! format; One activity-to-state mapping): the cells of the state's row up
//! to the first transparent one (rows are left-packed, short states pad
//! with empty cells), with `waiting` falling back to `idle` for an atlas
//! whose waiting row is empty.

use image::RgbaImage;
use image::imageops::{self, FilterType};

use crate::atlas::{Atlas, COLUMNS};
use crate::state::PetState;

/// Frames drawn per second.
pub const FPS: u32 = 8;
/// A pixel at or below this alpha is padding.
const BLANK_ALPHA: u8 = 8;

fn blank(cell: &RgbaImage) -> bool {
    cell.pixels().all(|p| p.0[3] <= BLANK_ALPHA)
}

/// The unscaled frames of `row`.
fn row_frames(atlas: &Atlas, row: u32) -> Vec<RgbaImage> {
    (0..COLUMNS)
        .map(|column| atlas.cell(row, column))
        .take_while(|cell| !blank(cell))
        .collect()
}

/// The frames of `state` at native size (empty when the row and its
/// fallback are empty).
#[must_use]
pub fn frames(atlas: &Atlas, state: PetState) -> Vec<RgbaImage> {
    let frames = row_frames(atlas, state.row());
    if frames.is_empty() && state != PetState::Idle {
        return row_frames(atlas, PetState::Idle.row());
    }
    frames
}

/// How many frames each state has (the desktop's sprite sheet steps).
#[must_use]
pub fn counts(atlas: &Atlas) -> Vec<(PetState, usize)> {
    PetState::ALL
        .into_iter()
        .map(|state| (state, row_frames(atlas, state.row()).len()))
        .collect()
}

/// `frames` resized to `width`x`height`.
#[must_use]
pub fn scaled(frames: &[RgbaImage], width: u32, height: u32) -> Vec<RgbaImage> {
    frames
        .iter()
        .map(|f| {
            if f.dimensions() == (width, height) {
                f.clone()
            } else {
                imageops::resize(f, width.max(1), height.max(1), FilterType::Triangle)
            }
        })
        .collect()
}

/// The smallest box holding every opaque pixel of `frames` (so a
/// terminal pet does not float in its transparent margin), as
/// `(x, y, width, height)`.
#[must_use]
pub fn opaque_box(frames: &[RgbaImage]) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for frame in frames {
        for (x, y, p) in frame.enumerate_pixels() {
            if p.0[3] > BLANK_ALPHA {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    bounds.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// `frames` cropped to their common opaque box.
#[must_use]
pub fn trimmed(frames: &[RgbaImage]) -> Vec<RgbaImage> {
    let Some((x, y, w, h)) = opaque_box(frames) else {
        return frames.to_vec();
    };
    frames
        .iter()
        .map(|f| imageops::crop_imm(f, x, y, w, h).to_image())
        .collect()
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A synthetic atlas: each state's row holds `n` opaque frames of one
    //! colour, the rest transparent.

    use image::{Rgba, RgbaImage};

    use crate::atlas::{Atlas, FRAME_H, FRAME_W, HEIGHT, WIDTH};

    /// An atlas with `frames[row]` filled cells per row.
    pub(crate) fn atlas(frames: [u32; 9]) -> Atlas {
        let mut image = RgbaImage::new(WIDTH, HEIGHT);
        for (row, &count) in (0_u32..).zip(frames.iter()) {
            for column in 0..count {
                let colour = Rgba([
                    u8::try_from(row * 25).unwrap_or(255),
                    u8::try_from(column * 30).unwrap_or(255),
                    200,
                    255,
                ]);
                // A centred body leaves a transparent margin.
                for y in 40..FRAME_H - 20 {
                    for x in 50..FRAME_W - 50 {
                        image.put_pixel(column * FRAME_W + x, row * FRAME_H + y, colour);
                    }
                }
            }
        }
        Atlas::from_image(image).unwrap().0
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::atlas;
    use super::*;

    #[test]
    fn a_row_stops_at_its_first_empty_cell() {
        let atlas = atlas([6, 0, 0, 4, 2, 3, 5, 8, 1]);
        assert_eq!(frames(&atlas, PetState::Idle).len(), 6);
        assert_eq!(frames(&atlas, PetState::Run).len(), 8);
        assert_eq!(frames(&atlas, PetState::Review).len(), 1);
    }

    #[test]
    fn an_empty_waiting_row_shows_idle() {
        let atlas = atlas([6, 0, 0, 4, 2, 3, 0, 8, 1]);
        assert_eq!(frames(&atlas, PetState::Waiting).len(), 6);
    }

    #[test]
    fn trimming_drops_the_transparent_margin() {
        let atlas = atlas([2; 9]);
        let idle = trimmed(&frames(&atlas, PetState::Idle));
        assert_eq!(idle[0].dimensions(), (92, 148));
    }
}
