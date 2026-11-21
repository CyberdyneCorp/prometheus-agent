//! The kitty graphics protocol: a PNG transmitted and placed at the
//! cursor under a fixed image and placement id, so the next frame
//! replaces it in place, and deleted by id.

use std::fmt::Write as _;
use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::{ImageFormat, RgbaImage};
use sha2::{Digest as _, Sha256};

/// A stable image id per pet in 1..=0x7fff (the same pet reuses its
/// terminal-side image).
pub(super) fn image_id(slug: &str) -> u32 {
    let digest = Sha256::digest(slug.as_bytes());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    n % 0x7ffe + 1
}

pub(super) fn png_base64(frame: &RgbaImage) -> String {
    let mut png = Vec::new();
    // Encoding an in-memory RGBA image cannot fail.
    let _ = frame.write_to(&mut Cursor::new(&mut png), ImageFormat::Png);
    STANDARD.encode(png)
}

/// `data` as APC chunks of at most 4096 bytes (`m=1`: more follow).
fn apc(control: &str, data: &str) -> String {
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    let last = chunks.len().saturating_sub(1);
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i != last);
        if i == 0 {
            let _ = write!(out, "\x1b_G{control},m={more};{chunk}\x1b\\");
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Transmit and place `frame` over `cols` x `rows` cells (`q=2`: no
/// replies that a reader would have to swallow).
pub(super) fn encode(frame: &RgbaImage, id: u32, cols: u16, rows: u16) -> String {
    apc(
        &format!("a=T,f=100,i={id},p=1,q=2,C=1,c={cols},r={rows}"),
        &png_base64(frame),
    )
}

/// Deletes the image and its placements.
pub(super) fn delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_frames_are_chunked_and_ids_are_stable() {
        let frame = RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 3, 255])
        });
        let escape = encode(&frame, image_id("boba"), 8, 4);
        assert!(escape.starts_with("\x1b_Ga=T,f=100,i="));
        assert!(escape.contains(",c=8,r=4,m="));
        assert!(escape.ends_with("\x1b\\"));
        assert_eq!(image_id("boba"), image_id("boba"));
        assert!((1..=0x7fff).contains(&image_id("mochi")));
        assert_eq!(delete(7), "\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
    }
}
