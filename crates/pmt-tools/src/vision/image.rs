//! Image bytes as the vision paths handle them: format sniffing, the
//! multi-frame limits, region crops at original resolution and SVG
//! rasterization (spec: web-and-media-tools, `vision_analyze` tool; Image
//! input routing).

use std::io::Cursor;

use base64::Engine as _;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder as _, ImageFormat};

/// Most bytes of one downloaded image.
pub const MAX_BYTES: usize = 50 * 1024 * 1024;
/// Most frames of an animated image.
pub const MAX_FRAMES: u64 = 100;
/// Most pixels over all frames of an animated image.
pub const MAX_PIXELS: u64 = 100_000_000;
/// Longest side of an SVG rasterization.
const MAX_RASTER_SIDE: f32 = 4_096.0;

/// MIME types every native image route accepts inline.
pub const RASTER: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image: its bytes and MIME type.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData {
    /// MIME type (`image/png`).
    pub mime: String,
    /// The encoded image.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageData")
            .field("mime", &self.mime)
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl ImageData {
    /// `bytes` with the MIME type their content shows.
    ///
    /// # Errors
    ///
    /// Not an image this build reads (PNG, JPEG, GIF, WebP or SVG).
    pub fn sniff(bytes: Vec<u8>) -> Result<Self, String> {
        let mime = mime_of(&bytes).ok_or("not a PNG, JPEG, GIF, WebP or SVG image")?;
        Ok(Self {
            mime: mime.to_owned(),
            bytes,
        })
    }

    /// The image of a `data:` URL.
    ///
    /// # Errors
    ///
    /// A malformed URL, non-base64 data or an unreadable image.
    pub fn from_data_url(url: &str) -> Result<Self, String> {
        let rest = url.strip_prefix("data:").ok_or("not a data: URL")?;
        let (header, data) = rest.split_once(',').ok_or("data: URL without ','")?;
        if !header.ends_with(";base64") {
            return Err("data: URL images must be base64".to_owned());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|err| format!("invalid base64 in data: URL: {err}"))?;
        if bytes.len() > MAX_BYTES {
            return Err(too_large());
        }
        Self::sniff(bytes)
    }

    /// Whether this is an SVG document.
    #[must_use]
    pub fn is_svg(&self) -> bool {
        self.mime == "image/svg+xml"
    }

    /// Width and height of the (first frame of the) image.
    ///
    /// # Errors
    ///
    /// An image that does not decode.
    pub fn dimensions(&self) -> Result<(u32, u32), String> {
        if self.is_svg() {
            let tree = svg_tree(&self.bytes)?;
            let size = tree.size().to_int_size();
            return Ok((size.width(), size.height()));
        }
        image::ImageReader::new(Cursor::new(&self.bytes))
            .with_guessed_format()
            .map_err(|err| err.to_string())?
            .into_dimensions()
            .map_err(|err| format!("unreadable image: {err}"))
    }

    /// Refuses animations over [`MAX_FRAMES`] frames or [`MAX_PIXELS`]
    /// pixels over all frames.
    ///
    /// # Errors
    ///
    /// The refusal text.
    pub fn check_frames(&self) -> Result<(), String> {
        let (width, height) = self.dimensions()?;
        let per_frame = u64::from(width) * u64::from(height);
        let frames = self.frame_count()?;
        if frames > MAX_FRAMES {
            return Err(format!(
                "animated image refused: more than {MAX_FRAMES} frames"
            ));
        }
        if frames > 1 && per_frame * frames > MAX_PIXELS {
            return Err(format!(
                "animated image refused: {} pixels over all frames (limit {MAX_PIXELS})",
                per_frame * frames
            ));
        }
        Ok(())
    }

    /// Frames of the image, counting at most `MAX_FRAMES + 1`.
    fn frame_count(&self) -> Result<u64, String> {
        let limit = usize::try_from(MAX_FRAMES + 1).unwrap_or(usize::MAX);
        let cursor = Cursor::new(self.bytes.as_slice());
        let count = match self.mime.as_str() {
            "image/gif" => GifDecoder::new(cursor)
                .map_err(|e| e.to_string())?
                .into_frames()
                .take(limit)
                .count(),
            "image/webp" => {
                let decoder = WebPDecoder::new(cursor).map_err(|e| e.to_string())?;
                if decoder.has_animation() {
                    decoder.into_frames().take(limit).count()
                } else {
                    1
                }
            }
            "image/png" => {
                let decoder = PngDecoder::new(cursor).map_err(|e| e.to_string())?;
                if decoder.is_apng().map_err(|e| e.to_string())? {
                    decoder
                        .apng()
                        .map_err(|e| e.to_string())?
                        .into_frames()
                        .take(limit)
                        .count()
                } else {
                    1
                }
            }
            _ => 1,
        };
        Ok(u64::try_from(count).unwrap_or(u64::MAX))
    }

    /// The `[x1, y1, x2, y2]` region at original resolution, as PNG.
    ///
    /// # Errors
    ///
    /// A region that is empty or outside the image, or an image that does
    /// not decode.
    pub fn crop(&self, region: [u32; 4]) -> Result<Self, String> {
        let source = if self.is_svg() {
            self.rasterized()?
        } else {
            self.clone()
        };
        let image = image::load_from_memory(&source.bytes)
            .map_err(|err| format!("unreadable image: {err}"))?;
        let [x1, y1, x2, y2] = region;
        if x2 <= x1 || y2 <= y1 || x2 > image.width() || y2 > image.height() {
            return Err(format!(
                "region [{x1}, {y1}, {x2}, {y2}] is not inside the {}x{} image",
                image.width(),
                image.height()
            ));
        }
        let cropped = image.crop_imm(x1, y1, x2 - x1, y2 - y1);
        let mut out = Cursor::new(Vec::new());
        cropped
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|err| format!("could not encode the crop: {err}"))?;
        Ok(Self {
            mime: "image/png".to_owned(),
            bytes: out.into_inner(),
        })
    }

    /// An SVG rendered to PNG (longest side at most 4096 pixels).
    ///
    /// # Errors
    ///
    /// An SVG that does not parse or render.
    pub fn rasterized(&self) -> Result<Self, String> {
        let tree = svg_tree(&self.bytes)?;
        let size = tree.size();
        let scale = (MAX_RASTER_SIDE / size.width().max(size.height())).min(1.0);
        let target = size
            .to_int_size()
            .scale_by(scale)
            .ok_or("SVG has no drawable size")?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(target.width(), target.height())
            .ok_or("SVG has no drawable size")?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let bytes = pixmap
            .encode_png()
            .map_err(|err| format!("could not encode the rasterized SVG: {err}"))?;
        Ok(Self {
            mime: "image/png".to_owned(),
            bytes,
        })
    }

    /// The image as base64 (inline request parts).
    #[must_use]
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
}

/// The refusal for an image over [`MAX_BYTES`].
#[must_use]
pub fn too_large() -> String {
    format!("image exceeds {} MiB", MAX_BYTES / (1024 * 1024))
}

fn svg_tree(bytes: &[u8]) -> Result<resvg::usvg::Tree, String> {
    resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default())
        .map_err(|err| format!("unreadable SVG: {err}"))
}

/// The MIME type of image `bytes`, by content.
fn mime_of(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => Some("image/png"),
        Ok(ImageFormat::Jpeg) => Some("image/jpeg"),
        Ok(ImageFormat::Gif) => Some("image/gif"),
        Ok(ImageFormat::WebP) => Some("image/webp"),
        _ => looks_like_svg(bytes).then_some("image/svg+xml"),
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    (head.starts_with("<?xml") || head.starts_with("<svg") || head.starts_with("<!DOCTYPE svg"))
        && head.contains("<svg")
}
