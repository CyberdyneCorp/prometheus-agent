//! The pet gallery (spec: pets, Pet gallery commands): a JSON manifest at
//! `pets.gallery_url` (default the public petdex gallery, whose manifest is
//! `{"pets": [{"slug", "displayName", "submittedBy", "spritesheetUrl",
//! "license"...}]}`) and the atlases it links. Everything is fetched over
//! HTTPS, read with a size cap and treated as untrusted.

use pmt_core::error::{Error, codes};
use serde::Deserialize;

use crate::atlas::MAX_BYTES;

/// The largest manifest read.
pub const MAX_MANIFEST_BYTES: usize = 16 << 20;

/// One gallery pet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    /// Its slug.
    pub slug: String,
    /// The display name.
    pub name: String,
    /// Who submitted it.
    pub author: String,
    /// The atlas URL.
    pub atlas_url: String,
    /// Its license, when given.
    pub license: String,
}

#[derive(Deserialize)]
struct Manifest {
    pets: Vec<Raw>,
}

/// One manifest entry. Every field is optional and may be an explicit
/// `null` (the live petdex manifest has `"submittedBy": null`), which
/// `#[serde(default)]` on a `String` would reject.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    submitted_by: Option<String>,
    #[serde(default)]
    spritesheet_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
}

/// The longest display name or author kept from a manifest.
pub const MAX_LABEL_CHARS: usize = 64;

/// A manifest label made safe to store and print: control and invisible
/// formatting characters (ESC, CR, bidi overrides, zero-width marks) are
/// dropped, so a hostile manifest cannot rewrite terminal output through
/// `pets list` or the stored `pet.toml` name, and it is capped at
/// [`MAX_LABEL_CHARS`].
fn label(text: Option<String>) -> String {
    text.unwrap_or_default()
        .chars()
        .filter(|&c| !c.is_control() && !invisible(c))
        .take(MAX_LABEL_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Zero-width, bidi-control and other invisible formatting characters.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::new(&codes::PET_GALLERY, message)
}

/// Whether `url` may be fetched: HTTPS, or (debug builds, for tests) plain
/// HTTP to the loopback address.
#[must_use]
pub fn allowed_url(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://127.0.0.1"))
}

/// The entries of a manifest document (entries without a slug or atlas
/// are skipped).
///
/// # Errors
///
/// `pet.gallery` when it is not a manifest.
pub fn parse(bytes: &[u8]) -> Result<Vec<Entry>, Error> {
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|e| unavailable(format!("the gallery manifest does not read: {e}")))?;
    Ok(manifest
        .pets
        .into_iter()
        .filter_map(|p| {
            let slug = p.slug.filter(|s| crate::store::valid_slug(s))?;
            let atlas_url = p.spritesheet_url.filter(|u| !u.is_empty())?;
            let name = label(p.display_name);
            Some(Entry {
                name: if name.is_empty() { slug.clone() } else { name },
                slug,
                author: label(p.submitted_by),
                atlas_url,
                license: label(p.license),
            })
        })
        .collect())
}

/// The entries whose slug or name contains `query` (case-insensitive),
/// at most `limit`.
#[must_use]
pub fn filter(entries: &[Entry], query: &str, limit: usize) -> Vec<Entry> {
    let query = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            query.is_empty()
                || e.slug.to_lowercase().contains(&query)
                || e.name.to_lowercase().contains(&query)
        })
        .take(limit)
        .cloned()
        .collect()
}

async fn get(client: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>, Error> {
    if !allowed_url(url) {
        return Err(unavailable(format!("refusing a non-HTTPS URL: {url}")));
    }
    let mut response = client
        .get(url)
        .header("user-agent", "prometheus-pets")
        .send()
        .await
        .map_err(|e| unavailable(format!("cannot reach {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(unavailable(format!(
            "{url} answered HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| unavailable(format!("reading {url} failed: {e}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > cap {
            // Read one byte past the cap: the atlas check names the size.
            body.truncate(cap + 1);
            break;
        }
    }
    Ok(body)
}

/// The host of `url` (lowercase, without credentials or port).
fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.strip_prefix('[').map_or_else(
        || host.split(':').next().unwrap_or(host),
        |v6| v6.split(']').next().unwrap_or(v6),
    );
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// The last two labels of a host (`assets.petdex.dev` → `petdex.dev`).
fn site(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 || host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_owned();
    }
    labels[labels.len() - 2..].join(".")
}

/// Whether `atlas` may be downloaded for a gallery at `gallery`: an
/// allowed URL on the gallery's own site, so a manifest cannot send the
/// download to another host (an internal service, say).
#[must_use]
pub fn same_site(gallery: &str, atlas: &str) -> bool {
    allowed_url(atlas)
        && matches!((host(gallery), host(atlas)), (Some(g), Some(a)) if site(&g) == site(&a))
}

/// The gallery's entries (those whose atlas is on another site are left
/// out).
///
/// # Errors
///
/// `pet.gallery`.
pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<Entry>, Error> {
    let body = get(client, url, MAX_MANIFEST_BYTES).await?;
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(unavailable("the gallery manifest is larger than 16 MiB"));
    }
    let mut entries = parse(&body)?;
    entries.retain(|e| same_site(url, &e.atlas_url));
    Ok(entries)
}

/// The atlas bytes of `entry` (at most 8 MiB plus one byte, so the atlas
/// check can refuse an oversized one).
///
/// # Errors
///
/// `pet.gallery`.
pub async fn download(client: &reqwest::Client, entry: &Entry) -> Result<Vec<u8>, Error> {
    get(client, &entry.atlas_url, MAX_BYTES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_parse_and_filter() {
        let doc = br#"{"generatedAt": "x", "pets": [
            {"slug": "boba", "displayName": "Boba", "submittedBy": "ana", "spritesheetUrl": "https://petdex.dev/boba.webp", "license": "CC0"},
            {"slug": "mochi", "spritesheetUrl": "https://petdex.dev/mochi.webp"},
            {"slug": "../evil", "spritesheetUrl": "https://petdex.dev/e.webp"},
            {"slug": "nosheet"}
        ]}"#;
        let entries = parse(doc).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "mochi");
        assert_eq!(filter(&entries, "BOB", 10)[0].slug, "boba");
        assert_eq!(filter(&entries, "", 1).len(), 1);
        assert_eq!(parse(b"[]").unwrap_err().code().code, "pet.gallery");
    }

    #[test]
    fn null_fields_keep_the_entry() {
        let doc = br#"{"pets": [
            {"slug": "boba", "displayName": null, "submittedBy": null, "spritesheetUrl": "https://petdex.dev/boba.webp", "license": null}
        ]}"#;
        let entries = parse(doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "boba");
        assert_eq!(entries[0].author, "");
        assert_eq!(entries[0].license, "");
    }

    #[test]
    fn labels_lose_terminal_controls_and_are_capped() {
        let long = "x".repeat(200);
        let doc = format!(
            r#"{{"pets": [{{"slug": "boba", "displayName": "Bo\u001b[2Jba\r\u202e", "submittedBy": "{long}", "spritesheetUrl": "https://petdex.dev/boba.webp"}}]}}"#
        );
        let entries = parse(doc.as_bytes()).unwrap();
        assert_eq!(entries[0].name, "Bo[2Jba");
        assert_eq!(entries[0].author.chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn atlases_come_from_the_gallerys_site_only() {
        let gallery = "https://petdex.dev/api/manifest";
        assert!(same_site(gallery, "https://petdex.dev/pets/boba.webp"));
        assert!(same_site(gallery, "https://cdn.petdex.dev/boba.webp"));
        assert!(!same_site(gallery, "https://evil.example/boba.webp"));
        assert!(!same_site(gallery, "https://169.254.169.254/latest"));
        assert!(!same_site(gallery, "http://petdex.dev/boba.webp"));
        assert!(!same_site(gallery, "https://user@internal.corp/x"));
    }

    #[test]
    fn only_https_is_fetched() {
        assert!(allowed_url("https://petdex.dev/api/manifest"));
        assert!(!allowed_url("http://petdex.dev/api/manifest"));
        assert!(!allowed_url("file:///etc/passwd"));
    }
}
