//! Bot metadata (spec: bot-mode, A bot is a profile with backend metadata;
//! Avatars; Roster presentation, hiding and sections): stored in the
//! profile's `state.db` (a store preference), read and written only
//! through `bot.meta.get` and `bot.meta.update`, so every client attached
//! to the daemon shows the same bot.

use base64::Engine as _;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{BotAvatar, BotMeta, BotSection, COLOURS, SHAPES, SILHOUETTES};
use pmt_store::Store;
use pmt_store::blobs::BlobRef;
use serde_json::{Map, Value};

/// The store preference holding the metadata.
const PREF: &str = "bot.meta";
/// What an uploaded avatar image may weigh.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// The blob reference owner of the avatar image.
const AVATAR_OWNER: &str = "bot_avatar";

/// The metadata fields `bot.meta.update` changes.
const FIELDS: [&str; 5] = ["title", "description", "avatar", "hidden", "section"];

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::RPC_INVALID_PARAMS, message)
}

/// The profile's metadata (the defaults when none was saved).
///
/// # Errors
///
/// Store errors.
pub fn read(store: &Store) -> Result<BotMeta, Error> {
    Ok(store
        .pref(PREF)?
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default())
}

fn write(store: &Store, meta: &BotMeta) -> Result<(), Error> {
    let mut saved = meta.clone();
    saved.groups.clear();
    let text = serde_json::to_string(&saved).map_err(|e| invalid(e.to_string()))?;
    store.set_pref(PREF, &text)
}

/// The title a bot shows: its own, else its profile name.
#[must_use]
pub fn title_of(profile: &str, meta: &BotMeta) -> String {
    meta.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| profile.to_owned())
}

/// Makes an avatar change concrete: an uploaded image is stored in the
/// blobs, a generated one is drawn first (by `generate`).
pub type Generate<'a> = &'a dyn Fn(&str) -> Result<(Vec<u8>, String), Error>;

/// Applies `changes` (each key replaces its field, `null` clears it) and
/// saves; the new metadata.
///
/// # Errors
///
/// `rpc.invalid_params` for an unknown field, an unknown avatar kind or
/// an invalid avatar; store errors.
pub fn update(
    store: &Store,
    changes: &Map<String, Value>,
    generate: Generate<'_>,
) -> Result<BotMeta, Error> {
    let mut meta = read(store)?;
    for (key, value) in changes {
        if !FIELDS.contains(&key.as_str()) {
            return Err(invalid(format!(
                "unknown bot metadata field '{key}' (known: {})",
                FIELDS.join(", ")
            )));
        }
        apply(store, &mut meta, key, value, generate)?;
    }
    write(store, &meta)?;
    Ok(meta)
}

fn text(value: &Value, key: &str) -> Result<Option<String>, Error> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
        _ => Err(invalid(format!("{key} must be a string or null"))),
    }
}

fn apply(
    store: &Store,
    meta: &mut BotMeta,
    key: &str,
    value: &Value,
    generate: Generate<'_>,
) -> Result<(), Error> {
    match key {
        "title" => meta.title = text(value, key)?,
        "description" => meta.description = text(value, key)?,
        "hidden" => {
            meta.hidden = value
                .as_bool()
                .ok_or_else(|| invalid("hidden must be true or false"))?;
        }
        "section" => {
            meta.section = match value {
                Value::Null => None,
                _ => Some(
                    serde_json::from_value::<BotSection>(value.clone())
                        .map_err(|e| invalid(format!("section: {e}")))?,
                ),
            };
        }
        _ => {
            let avatar = match value {
                Value::Null => BotAvatar::default(),
                _ => serde_json::from_value::<BotAvatar>(value.clone())
                    .map_err(|e| invalid(format!("avatar: {e}")))?,
            };
            meta.avatar = settle(store, avatar, generate)?;
        }
    }
    Ok(())
}

/// Validates `avatar` and stores its image.
fn settle(store: &Store, avatar: BotAvatar, generate: Generate<'_>) -> Result<BotAvatar, Error> {
    match avatar {
        BotAvatar::Blob { silhouette, seed } => {
            if let Some(s) = &silhouette
                && !SILHOUETTES.contains(&s.as_str())
            {
                return Err(invalid(format!(
                    "unknown silhouette '{s}' (one of {})",
                    SILHOUETTES.join(", ")
                )));
            }
            Ok(BotAvatar::Blob { seed, silhouette })
        }
        BotAvatar::Geometric { shape, color } => {
            if !SHAPES.contains(&shape.as_str()) || !COLOURS.contains(&color.as_str()) {
                return Err(invalid(format!(
                    "a geometric avatar is one of {} in one of {}",
                    SHAPES.join(", "),
                    COLOURS.join(", ")
                )));
            }
            Ok(BotAvatar::Geometric { shape, color })
        }
        BotAvatar::Image { data, blob, mime } => {
            let bytes = match (data, blob) {
                (Some(data), _) => base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .map_err(|e| invalid(format!("avatar image data is not base64: {e}")))?,
                (None, Some(blob)) => return keep_image(store, &blob, mime),
                (None, None) => return Err(invalid("an image avatar needs its data")),
            };
            store_image(store, &bytes, &mime)
        }
        BotAvatar::Generated { prompt } => {
            let (bytes, mime) = generate(&prompt)?;
            store_image(store, &bytes, &mime)
        }
        BotAvatar::Pet { id } if !id.trim().is_empty() => Ok(BotAvatar::Pet { id }),
        BotAvatar::Pet { .. } => Err(invalid("a pet avatar needs a companion id")),
    }
}

/// An image the profile already holds.
fn keep_image(store: &Store, blob: &str, mime: String) -> Result<BotAvatar, Error> {
    let reference = BlobRef::parse(&format!("blob:sha256:{blob}"))
        .ok_or_else(|| invalid("an avatar blob is a SHA-256"))?;
    store.get_blob(&reference)?;
    Ok(BotAvatar::Image {
        blob: Some(blob.to_owned()),
        mime,
        data: None,
    })
}

/// The image type `bytes` start with.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn store_image(store: &Store, bytes: &[u8], declared: &str) -> Result<BotAvatar, Error> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(invalid(format!(
            "an avatar image is at most 5 MiB (this one is {} bytes)",
            bytes.len()
        )));
    }
    let mime = sniff(bytes).ok_or_else(|| invalid("an avatar image is PNG, JPEG or WebP"))?;
    if !declared.is_empty() && declared != mime {
        return Err(invalid(format!(
            "the avatar image is {mime}, not {declared}"
        )));
    }
    let blob = store.put_blob(bytes)?;
    store.reference_blob(&blob, AVATAR_OWNER, "meta")?;
    Ok(BotAvatar::Image {
        blob: Some(blob.sha256().to_owned()),
        mime: mime.to_owned(),
        data: None,
    })
}

/// The seed of the blob face of a bot named `name` (FNV-1a over the
/// lower-cased name): the same name always yields the same face. The
/// desktop draws the face from this seed and its silhouette.
#[must_use]
pub fn face_seed(name: &str) -> u32 {
    name.to_lowercase()
        .bytes()
        .fold(0x811c_9dc5_u32, |hash, b| {
            (hash ^ u32::from(b)).wrapping_mul(0x0100_0193)
        })
}

/// `avatar` as every client draws it: a blob face resolved to its seed
/// and silhouette (the locked ones, else the name's).
#[must_use]
pub fn resolved(name: &str, avatar: &BotAvatar) -> BotAvatar {
    match avatar {
        BotAvatar::Blob { seed, silhouette } => {
            let seed = seed.unwrap_or_else(|| face_seed(name));
            let silhouette = silhouette
                .clone()
                .unwrap_or_else(|| SILHOUETTES[usize::try_from(seed % 6).unwrap_or(0)].to_owned());
            BotAvatar::Blob {
                seed: Some(seed),
                silhouette: Some(silhouette),
            }
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Deterministic default face.
    #[test]
    fn the_same_name_always_yields_the_same_face() {
        let a = resolved("scribe", &BotAvatar::default());
        let b = resolved("Scribe", &BotAvatar::default());
        assert_eq!(a, b);
        assert_eq!(face_seed("scribe"), 0xfd11_18f3);
        assert_ne!(face_seed("scribe"), face_seed("writer"));
        let locked = resolved(
            "scribe",
            &BotAvatar::Blob {
                seed: Some(7),
                silhouette: None,
            },
        );
        assert_eq!(
            locked,
            BotAvatar::Blob {
                seed: Some(7),
                silhouette: Some("organic".into())
            }
        );
    }

    #[test]
    fn images_are_sniffed_by_their_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"GIF89a"), None);
    }
}
