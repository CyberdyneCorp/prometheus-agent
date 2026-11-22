//! A pet gallery for a test home: a wiremock server serving a petdex-style
//! manifest and atlases, with `pets.gallery_url` pointing at it.

use std::io::Cursor;
use std::path::Path;

use image::{ImageFormat, Rgba, RgbaImage};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{ok, run};

/// A valid 1536x1872 atlas: every row holds `frames` opaque cells.
pub fn atlas(frames: u32) -> Vec<u8> {
    sized(1536, 1872, frames)
}

/// A PNG of `width`x`height` with `frames` opaque 192x208 cells per row.
pub fn sized(width: u32, height: u32, frames: u32) -> Vec<u8> {
    let image = RgbaImage::from_fn(width, height, |x, y| {
        let (cell, cx, cy) = (x / 192, x % 192, y % 208);
        if cell < frames && (30..162).contains(&cx) && (20..200).contains(&cy) {
            Rgba([200, 90 + u8::try_from(y / 208 * 15).unwrap_or(0), 40, 255])
        } else {
            Rgba([0, 0, 0, 0])
        }
    });
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

pub struct Gallery {
    pub runtime: tokio::runtime::Runtime,
    pub server: MockServer,
}

impl Gallery {
    /// Serves `pets` (slug, atlas) and points `home`'s default profile at
    /// the manifest.
    pub fn start(home: &Path, pets: &[(&str, Vec<u8>)]) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        let entries: Vec<_> = pets
            .iter()
            .map(|(slug, _)| {
                json!({
                    "slug": slug,
                    "displayName": slug.to_uppercase(),
                    "submittedBy": "tester",
                    "spritesheetUrl": format!("{}/atlas/{slug}.png", server.uri()),
                    "license": "CC0",
                })
            })
            .collect();
        let manifest =
            json!({"generatedAt": "2026-11-20", "total": entries.len(), "pets": entries});
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/manifest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(manifest))
                .mount(&server)
                .await;
            for (slug, bytes) in pets {
                Mock::given(method("GET"))
                    .and(path(format!("/atlas/{slug}.png")))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                    .mount(&server)
                    .await;
            }
        });
        let url = format!("{}/api/manifest", server.uri());
        ok(&run(home, &["config", "set", "pets.gallery_url", &url]));
        Self { runtime, server }
    }
}

/// Installs `slugs` from a gallery serving them.
pub fn install(home: &Path, slugs: &[&str]) -> Gallery {
    let pets: Vec<(&str, Vec<u8>)> = slugs.iter().map(|s| (*s, atlas(6))).collect();
    let gallery = Gallery::start(home, &pets);
    for slug in slugs {
        ok(&run(home, &["pets", "install", slug]));
    }
    gallery
}
