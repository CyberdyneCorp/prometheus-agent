//! Packing generated rows into an atlas. Each animation row comes back
//! from the image backend as one picture of eight poses side by side on a
//! flat magenta background; it is fitted to 1536x208, the background is
//! keyed out, and the nine rows are stacked in the petdex order.

use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};

use crate::atlas::{Atlas, AtlasError, FRAME_H, HEIGHT, ROWS, WIDTH};

/// The rows in atlas order and the pose each one asks for.
pub const ROW_POSES: [(&str, &str); ROWS as usize] = [
    ("idle", "standing still, breathing gently"),
    ("running-right", "running to the right"),
    ("running-left", "running to the left"),
    ("waving", "waving hello with one paw or hand"),
    ("jumping", "jumping for joy"),
    ("failed", "sad and deflated after a mistake"),
    ("waiting", "waiting patiently, looking up expectantly"),
    ("running", "running in place, busy working"),
    ("review", "reading a small book, thinking hard"),
];

/// Whether a pixel is the flat background (close to pure magenta).
fn background(p: Rgba<u8>) -> bool {
    let [r, g, b, _] = p.0;
    r > 200 && b > 200 && g < 80
}

/// `row` (any size) fitted to one atlas row with its background keyed
/// out.
#[must_use]
pub fn fit_row(row: &RgbaImage) -> RgbaImage {
    let mut fitted = imageops::resize(row, WIDTH, FRAME_H, FilterType::Triangle);
    for p in fitted.pixels_mut() {
        if background(*p) {
            *p = Rgba([0, 0, 0, 0]);
        }
    }
    fitted
}

/// The atlas of nine rows (in [`ROW_POSES`] order) and its PNG file.
///
/// # Errors
///
/// [`AtlasError`] when not nine rows are given.
pub fn pack(rows: &[RgbaImage]) -> Result<(Atlas, Vec<u8>), AtlasError> {
    if rows.len() != ROWS as usize {
        return Err(AtlasError(format!(
            "expected {ROWS} animation rows, got {}",
            rows.len()
        )));
    }
    let mut atlas = RgbaImage::new(WIDTH, HEIGHT);
    for (index, row) in (0_u32..).zip(rows) {
        imageops::replace(&mut atlas, &fit_row(row), 0, i64::from(index * FRAME_H));
    }
    Atlas::from_image(atlas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PetState;

    #[test]
    fn nine_rows_pack_into_a_valid_atlas_with_the_background_keyed_out() {
        let row = RgbaImage::from_fn(800, 100, |x, _| {
            if x % 100 < 60 {
                Rgba([30, 120, 40, 255])
            } else {
                Rgba([255, 0, 255, 255])
            }
        });
        let (atlas, png) = pack(&vec![row; 9]).unwrap();
        assert_eq!(Atlas::decode(&png).unwrap().sha256(), atlas.sha256());
        assert_eq!(crate::frames::frames(&atlas, PetState::Run).len(), 8);
        let corner = atlas.image().get_pixel(WIDTH - 1, 0);
        assert_eq!(corner.0[3], 0, "the magenta background is transparent");
        assert!(pack(&[]).is_err());
    }
}
