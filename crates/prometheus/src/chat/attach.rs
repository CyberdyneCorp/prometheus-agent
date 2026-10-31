//! Image attachments of the composer (spec: cli, Image attachments):
//! `chat --image <path>` for the first prompt and `/image <path>` for the
//! next one. An image is checked when it is attached (PNG, JPEG, GIF or
//! WebP up to [`MAX_BYTES`]), stored in the session blob store when the
//! prompt is sent, and delivered as pixels or, for a model that cannot
//! see, as the `vision` auxiliary route's description. A model without
//! vision and without a configured vision route refuses the prompt before
//! any provider call.

use std::path::{Path, PathBuf};

use pmt_agent::turn::transcript::ImageRef;
use pmt_core::error::{Error, codes};
use pmt_providers::resolve::Trace;
use pmt_tools::vision::routing::{RoutingInputs, aux_explicit};
use pmt_tools::vision::{Delivery, Describer as _, ImageData};

use super::session::{Factory, Live};

/// Largest attachment.
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Formats an attachment may have.
const ACCEPTED: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// What the owner of an attachment's blob reference is called.
const OWNER: &str = "session_attachment";

/// One checked image file.
#[derive(Debug, Clone)]
pub struct Image {
    /// The path as given.
    pub path: PathBuf,
    /// The file name shown in notes.
    pub name: String,
    /// The bytes and their MIME type.
    pub data: ImageData,
}

/// Reads and checks `path`.
///
/// # Errors
///
/// `chat.image`: missing, unreadable, too large or not an accepted format.
pub fn load(path: &Path) -> Result<Image, Error> {
    let fail = |why: String| {
        Error::new(
            &codes::CHAT_IMAGE,
            format!("cannot attach {}: {why}", path.display()),
        )
    };
    let size = std::fs::metadata(path)
        .map_err(|e| fail(e.to_string()))?
        .len();
    if size > MAX_BYTES {
        return Err(fail("larger than 20 MiB".to_owned()));
    }
    let bytes = std::fs::read(path).map_err(|e| fail(e.to_string()))?;
    let not_accepted = || fail("not a PNG, JPEG, GIF or WebP image".to_owned());
    let data = ImageData::sniff(bytes).map_err(|_| not_accepted())?;
    if !ACCEPTED.contains(&data.mime.as_str()) {
        return Err(not_accepted());
    }
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Ok(Image {
        path: path.to_owned(),
        name,
        data,
    })
}

/// Stores `images` for `live`'s next message and decides their delivery.
///
/// # Errors
///
/// `chat.image_unsupported` (exit 78) when the model cannot see and no
/// vision route is configured; resolution and store errors.
pub async fn prepare(
    factory: &Factory,
    live: &Live,
    images: &[Image],
) -> Result<Vec<ImageRef>, Error> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    let models = &factory.models;
    let route = models
        .resolver()
        .resolve_target(&live.model.target(), &mut Trace::default())
        .await?;
    let (meta, _) = models.catalog().metadata(&route.provider, &route.model);
    let config = factory.scope.config();
    let explicit = aux_explicit(config);
    if !meta.supports_vision && !explicit {
        return Err(Error::new(
            &codes::CHAT_IMAGE_UNSUPPORTED,
            format!("model {} does not accept images", route.model),
        ));
    }
    let delivery = RoutingInputs {
        mode: config.vision.image_input_mode,
        main_vision: meta.supports_vision,
        aux_explicit: explicit,
    }
    .delivery();
    let describer = match delivery {
        Delivery::Native => None,
        Delivery::Text => models.describer(&route, live.id).await?,
    };
    let mut refs = Vec::with_capacity(images.len());
    for image in images {
        let blob = factory.store.put_blob(&image.data.bytes)?;
        factory
            .store
            .reference_blob(&blob, OWNER, &live.id.to_string())?;
        let description = match (&describer, delivery) {
            (_, Delivery::Native) => None,
            (None, Delivery::Text) => Some("no vision route can describe it".to_owned()),
            (Some(describer), Delivery::Text) => Some(
                describer
                    .describe(&image.data, pmt_agent::vision::DESCRIBE_QUESTION)
                    .await
                    .unwrap_or_else(|why| format!("could not be described: {why}")),
            ),
        };
        refs.push(ImageRef {
            blob: blob.to_string(),
            media_type: image.data.mime.clone(),
            name: image.name.clone(),
            description,
        });
    }
    Ok(refs)
}
