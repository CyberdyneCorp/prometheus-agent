//! `pets.scale` and terminal widths (spec: pets, Pet configuration and
//! scale; Terminal pet rendering): one scale for every surface, clamped to
//! 0.1-3.0; half-block pets keep a minimum legible width.

use pmt_config::schema::{DEFAULT_PET_SCALE, MAX_PET_SCALE, MIN_PET_SCALE};

use crate::atlas::FRAME_W;

/// Terminal columns a native cell spans at scale 1 (about 8 px a cell).
pub const BASE_COLS: u32 = FRAME_W / 8;
/// The narrowest half-block pet that still reads as a pet.
pub const MIN_UNICODE_COLS: u32 = 16;

/// `scale` within 0.1-3.0, and whether it was changed (a non-finite
/// value becomes the default).
#[must_use]
pub fn clamp_scale(scale: f64) -> (f64, bool) {
    if !scale.is_finite() {
        return (DEFAULT_PET_SCALE, true);
    }
    let clamped = scale.clamp(MIN_PET_SCALE, MAX_PET_SCALE);
    (clamped, (clamped - scale).abs() > f64::EPSILON)
}

/// The columns of a half-block pet: `unicode_cols` when set, else derived
/// from `scale`, never below [`MIN_UNICODE_COLS`].
#[must_use]
pub fn unicode_cols(scale: f64, unicode_cols: u32) -> u32 {
    if unicode_cols > 0 {
        return unicode_cols;
    }
    let (scale, _) = clamp_scale(scale);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // within 2..73
    let derived = (f64::from(BASE_COLS) * scale).round() as u32;
    derived.max(MIN_UNICODE_COLS)
}

/// The pixel size of a cell drawn at `scale`.
#[must_use]
pub fn scaled_cell(scale: f64) -> (u32, u32) {
    let (scale, _) = clamp_scale(scale);
    let px = |n: u32| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // within 19..625
        let v = (f64::from(n) * scale).round() as u32;
        v.max(1)
    };
    (px(FRAME_W), px(crate::atlas::FRAME_H))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_clamped_and_says_so() {
        assert_eq!(clamp_scale(5.0), (3.0, true));
        assert_eq!(clamp_scale(0.01), (0.1, true));
        assert_eq!(clamp_scale(0.5), (0.5, false));
        assert_eq!(clamp_scale(f64::NAN), (DEFAULT_PET_SCALE, true));
    }

    #[test]
    fn half_blocks_keep_a_legible_width() {
        assert_eq!(unicode_cols(0.1, 0), MIN_UNICODE_COLS);
        assert_eq!(unicode_cols(1.0, 0), 24);
        assert_eq!(unicode_cols(2.0, 0), 48);
        assert_eq!(unicode_cols(0.1, 10), 10);
        assert_eq!(scaled_cell(1.0), (192, 208));
    }
}
