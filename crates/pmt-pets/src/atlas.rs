//! The pet atlas (spec: pets, Pet storage and sprite format): one image in
//! the petdex layout, 8 columns by 9 rows of 192x208 pixel cells, one row
//! per animation state. Atlases come from the gallery or an image backend,
//! so they are untrusted: only PNG or WebP of at most [`MAX_BYTES`] whose
//! header names exactly [`WIDTH`]x[`HEIGHT`] is decoded, and the decoder
//! may allocate no more than one RGBA copy of that size.

use std::fmt;
use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use sha2::{Digest as _, Sha256};

/// A cell's width in pixels.
pub const FRAME_W: u32 = 192;
/// A cell's height in pixels.
pub const FRAME_H: u32 = 208;
/// Cells per row (animation frames of a state).
pub const COLUMNS: u32 = 8;
/// Rows (animation states).
pub const ROWS: u32 = 9;
/// The atlas width.
pub const WIDTH: u32 = FRAME_W * COLUMNS;
/// The atlas height.
pub const HEIGHT: u32 = FRAME_H * ROWS;
/// The largest atlas file accepted.
pub const MAX_BYTES: usize = 8 << 20;

/// Why an atlas was refused: displays as `invalid pet atlas: <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasError(pub String);

impl fmt::Display for AtlasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid pet atlas: {}", self.0)
    }
}

impl std::error::Error for AtlasError {}

impl From<AtlasError> for pmt_core::error::Error {
    fn from(err: AtlasError) -> Self {
        Self::new(&pmt_core::error::codes::PET_INVALID_ATLAS, err.to_string())
    }
}

/// The file format of an atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// PNG.
    Png,
    /// WebP.
    Webp,
}

impl Format {
    /// The file extension the store writes.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }

    /// The MIME type.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

/// A validated, decoded atlas.
#[derive(Debug, Clone)]
pub struct Atlas {
    image: RgbaImage,
    format: Format,
    sha256: String,
}

fn refuse(reason: impl Into<String>) -> AtlasError {
    AtlasError(reason.into())
}

/// The SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

impl Atlas {
    /// Validates and decodes `bytes`.
    ///
    /// # Errors
    ///
    /// [`AtlasError`] naming the first check that failed: the size, the
    /// format, the dimensions or the image data.
    pub fn decode(bytes: &[u8]) -> Result<Self, AtlasError> {
        if bytes.len() > MAX_BYTES {
            return Err(refuse(format!(
                "{} bytes is more than the 8 MiB limit",
                bytes.len()
            )));
        }
        let format = match image::guess_format(bytes) {
            Ok(ImageFormat::Png) => Format::Png,
            Ok(ImageFormat::WebP) => Format::Webp,
            _ => return Err(refuse("not a PNG or WebP image")),
        };
        let (width, height) = reader(bytes, format)
            .into_dimensions()
            .map_err(|e| refuse(format!("unreadable image header: {e}")))?;
        if (width, height) != (WIDTH, HEIGHT) {
            return Err(refuse(format!(
                "expected {WIDTH}x{HEIGHT}, got {width}x{height}"
            )));
        }
        let mut decoder = reader(bytes, format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(WIDTH);
        limits.max_image_height = Some(HEIGHT);
        // One RGBA copy, plus room for the decoder's own row buffers.
        limits.max_alloc = Some(u64::from(WIDTH) * u64::from(HEIGHT) * 4 * 2);
        decoder.limits(limits);
        let image = decoder
            .decode()
            .map_err(|e| refuse(format!("unreadable image data: {e}")))?
            .into_rgba8();
        Ok(Self {
            image,
            format,
            sha256: sha256_hex(bytes),
        })
    }

    /// Wraps an image already in memory (a generated atlas).
    ///
    /// # Errors
    ///
    /// [`AtlasError`] when it does not measure [`WIDTH`]x[`HEIGHT`].
    pub fn from_image(image: RgbaImage) -> Result<(Self, Vec<u8>), AtlasError> {
        if image.dimensions() != (WIDTH, HEIGHT) {
            let (w, h) = image.dimensions();
            return Err(refuse(format!("expected {WIDTH}x{HEIGHT}, got {w}x{h}")));
        }
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .map_err(|e| refuse(format!("cannot encode: {e}")))?;
        let sha256 = sha256_hex(&png);
        Ok((
            Self {
                image,
                format: Format::Png,
                sha256,
            },
            png,
        ))
    }

    /// The decoded pixels.
    #[must_use]
    pub fn image(&self) -> &RgbaImage {
        &self.image
    }

    /// The file format.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// The SHA-256 of the file, lowercase hex.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The cell at `row`, `column` (clamped to the grid).
    #[must_use]
    pub fn cell(&self, row: u32, column: u32) -> RgbaImage {
        let (row, column) = (row.min(ROWS - 1), column.min(COLUMNS - 1));
        image::imageops::crop_imm(
            &self.image,
            column * FRAME_W,
            row * FRAME_H,
            FRAME_W,
            FRAME_H,
        )
        .to_image()
    }
}

fn reader(bytes: &[u8], format: Format) -> ImageReader<Cursor<&[u8]>> {
    let format = match format {
        Format::Png => ImageFormat::Png,
        Format::Webp => ImageFormat::WebP,
    };
    ImageReader::with_format(Cursor::new(bytes), format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        RgbaImage::new(width, height)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn the_layout_is_8_by_9_cells_of_192_by_208() {
        assert_eq!((WIDTH, HEIGHT), (1536, 1872));
        let atlas = Atlas::decode(&png(WIDTH, HEIGHT)).unwrap();
        assert_eq!(atlas.format(), Format::Png);
        assert_eq!(atlas.sha256().len(), 64);
        assert_eq!(atlas.cell(8, 7).dimensions(), (FRAME_W, FRAME_H));
    }

    /// Scenario: Wrong dimensions.
    #[test]
    fn wrong_dimensions_are_refused_with_the_expected_size() {
        let err = Atlas::decode(&png(1000, 1000)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("invalid pet atlas: expected 1536x1872"),
            "{err}"
        );
    }

    #[test]
    fn other_formats_and_oversized_files_are_refused() {
        let mut jpeg = Vec::new();
        image::RgbImage::new(4, 4)
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(
            Atlas::decode(&jpeg).unwrap_err().to_string(),
            "invalid pet atlas: not a PNG or WebP image"
        );
        assert_eq!(
            Atlas::decode(b"<html>").unwrap_err().to_string(),
            "invalid pet atlas: not a PNG or WebP image"
        );
        let big = vec![0_u8; MAX_BYTES + 1];
        assert!(Atlas::decode(&big).unwrap_err().0.contains("8 MiB"));
    }

    #[test]
    fn a_truncated_image_is_refused_not_decoded() {
        let mut bytes = png(WIDTH, HEIGHT);
        bytes.truncate(bytes.len() / 2);
        let err = Atlas::decode(&bytes).unwrap_err();
        assert!(err.0.starts_with("unreadable image"), "{err}");
    }
}
