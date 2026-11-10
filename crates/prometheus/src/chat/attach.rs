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
use pmt_config::schema::AttachmentStorage;
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
const OWNER: &str = pmt_store::portable::SESSION_ATTACHMENT;

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

/// Where `attachments.storage = "workspace"` copies a session's
/// attachments: `<cwd>/.prometheus/attachments/<session_id>/`.
#[must_use]
pub fn workspace_dir(cwd: &Path, session: &str) -> PathBuf {
    cwd.join(".prometheus").join("attachments").join(session)
}

/// Keeps a received non-image file (a messaging attachment) for `live`:
/// stored in the blob store under the session, and copied into the
/// workspace with `attachments.storage = "workspace"` (the copy's path is
/// returned).
///
/// # Errors
///
/// `chat.image` when the file cannot be read or copied; store errors.
pub fn keep_file(factory: &Factory, live: &Live, path: &Path) -> Result<Option<PathBuf>, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(&codes::CHAT_IMAGE, path, &e))?;
    let blob = factory.store.put_blob(&bytes)?;
    factory
        .store
        .reference_blob(&blob, OWNER, &live.id.to_string())?;
    if factory.scope.config().attachments.storage != AttachmentStorage::Workspace {
        return Ok(None);
    }
    let name = path.file_name().map_or_else(
        || "attachment".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    workspace_copy(&live.cwd(), &live.id.to_string(), &name, &bytes).map(Some)
}

/// Copies `bytes` as `name` into the session's workspace attachment
/// directory (the blob store stays the canonical reference); a name
/// already taken gets a numbered prefix.
fn workspace_copy(cwd: &Path, session: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, Error> {
    let dir = workspace_dir(cwd, session);
    let fail = |e: std::io::Error| Error::io(&codes::CHAT_IMAGE, &dir, &e);
    std::fs::create_dir_all(&dir).map_err(fail)?;
    let safe: String = name
        .chars()
        .map(|c| if std::path::is_separator(c) { '_' } else { c })
        .collect();
    let mut path = dir.join(&safe);
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("{n}-{safe}"));
    }
    std::fs::write(&path, bytes).map_err(fail)?;
    Ok(path)
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
        if config.attachments.storage == AttachmentStorage::Workspace {
            workspace_copy(
                &live.cwd(),
                &live.id.to_string(),
                &image.name,
                &image.data.bytes,
            )?;
        }
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
