//! The image embed budget and the per-image cap (spec: web-and-media-tools,
//! Image embed budget, per-image cap and replay).
//!
//! A natively embedded image larger than `vision.embed_target_bytes`
//! (clamped to 64 KiB..4 MiB) or wider than [`MAX_WIDTH`] pixels is
//! re-encoded as a JPEG that fits: first narrowed to [`MAX_WIDTH`], then
//! at falling quality, then smaller. The encoding is deterministic, so the
//! same stored image always becomes the same bytes (a replayed image is
//! byte-identical to its first send).

use std::io::Cursor;

use image::DynamicImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

use super::ImageData;

/// Widest embedded image, in pixels.
pub const MAX_WIDTH: u32 = 1_568;
/// Smallest and largest embed target.
pub const TARGET_RANGE: (u64, u64) = (65_536, 4_194_304);

/// JPEG qualities tried before the image is made smaller.
const QUALITIES: [u8; 4] = [85, 70, 55, 40];
/// Scale steps after the qualities are spent.
const SHRINKS: usize = 8;

/// `configured` within [`TARGET_RANGE`].
#[must_use]
pub const fn clamp_target(configured: u64) -> u64 {
    if configured < TARGET_RANGE.0 {
        TARGET_RANGE.0
    } else if configured > TARGET_RANGE.1 {
        TARGET_RANGE.1
    } else {
        configured
    }
}

impl ImageData {
    /// The image within `target` bytes and [`MAX_WIDTH`] pixels: itself
    /// when it already fits, else a JPEG that does.
    ///
    /// # Errors
    ///
    /// An unreadable image, or one that cannot be made to fit.
    pub fn fitted(self, target: u64) -> Result<Self, String> {
        let (width, _) = self.dimensions()?;
        let size = u64::try_from(self.bytes.len()).unwrap_or(u64::MAX);
        if size <= target && width <= MAX_WIDTH {
            return Ok(self);
        }
        let decoded = image::load_from_memory(&self.bytes)
            .map_err(|err| format!("unreadable image: {err}"))?;
        let mut image = if decoded.width() > MAX_WIDTH {
            decoded.resize(MAX_WIDTH, u32::MAX, FilterType::Triangle)
        } else {
            decoded
        };
        for quality in QUALITIES {
            let bytes = jpeg(&image, quality)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= target {
                return Ok(Self::jpeg_of(bytes));
            }
        }
        for _ in 0..SHRINKS {
            let (w, h) = (image.width() * 3 / 4, image.height() * 3 / 4);
            if w == 0 || h == 0 {
                break;
            }
            image = image.resize_exact(w, h, FilterType::Triangle);
            let bytes = jpeg(&image, QUALITIES[2])?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= target {
                return Ok(Self::jpeg_of(bytes));
            }
        }
        Err(format!(
            "the image cannot be made smaller than {target} bytes"
        ))
    }

    fn jpeg_of(bytes: Vec<u8>) -> Self {
        Self {
            mime: "image/jpeg".to_owned(),
            bytes,
        }
    }
}

fn jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&image.to_rgb8())
        .map_err(|err| format!("could not encode the image: {err}"))?;
    Ok(out.into_inner())
}

/// The cap of `vision_analyze` embeds per image in a session:
/// `configured` (`0` = unlimited), unset = 3 in a delegated child and
/// unlimited for the main agent.
#[must_use]
pub fn per_image_cap(configured: Option<u32>, delegated: bool) -> Option<u32> {
    match configured {
        Some(0) => None,
        Some(cap) => Some(cap),
        None => delegated.then_some(3),
    }
}

/// The refusal past the cap.
#[must_use]
pub fn refusal(loads: u32) -> String {
    format!(
        "vision_analyze refused: this image has already been loaded into context {loads} time(s)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise_png(width: u32, height: u32) -> ImageData {
        let mut seed = 7u32;
        let image = image::RgbImage::from_fn(width, height, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        ImageData::sniff(out.into_inner()).unwrap()
    }

    #[test]
    fn big_or_wide_images_become_a_fitting_jpeg_deterministically() {
        let big = noise_png(2_000, 600);
        assert!(big.bytes.len() > 262_144);
        let fitted = big.clone().fitted(262_144).unwrap();
        assert_eq!(fitted.mime, "image/jpeg");
        assert!(fitted.bytes.len() <= 262_144);
        assert!(fitted.dimensions().unwrap().0 <= MAX_WIDTH);
        assert_eq!(
            big.fitted(262_144).unwrap(),
            fitted,
            "same input, same bytes"
        );
        let small = noise_png(40, 40);
        assert_eq!(
            small.clone().fitted(262_144).unwrap(),
            small,
            "fits as it is"
        );
    }

    #[test]
    fn the_target_is_clamped_and_the_cap_defaults_by_agent() {
        assert_eq!(clamp_target(1), 65_536);
        assert_eq!(clamp_target(u64::MAX), 4_194_304);
        assert_eq!(per_image_cap(None, true), Some(3));
        assert_eq!(per_image_cap(None, false), None);
        assert_eq!(per_image_cap(Some(0), true), None);
        assert_eq!(per_image_cap(Some(5), false), Some(5));
    }
}
