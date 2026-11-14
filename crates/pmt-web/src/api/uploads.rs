//! Chat attachments (spec: web-dashboard, Upload and download limits): an
//! upload is refused (413) by its declared length or while it streams,
//! before anything reaches the blob store, once it passes
//! `dashboard.max_upload_bytes`; a download serves only a blob that a
//! session of the addressed profile references, as an attachment.
//!
//! The webapp stages every file a user selects, pastes or drops through
//! `POST stage` (spec: web-dashboard, Webapp file staging and terminals):
//! the same flow capped at 16 MiB per file.

use hyper::body::Incoming;
use hyper::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, HeaderValue};
use pmt_store::BlobRef;
use serde_json::json;

use super::profile::Profile;
use crate::Shared;
use crate::http::{Reply, Req, bytes, error, from_error, json_reply, read_body};

/// The cap on one staged webapp file.
pub(crate) const MAX_STAGED_BYTES: u64 = 16 * 1024 * 1024;

fn too_large(max: u64, what: &str) -> Reply {
    error(
        413,
        "too_large",
        &format!("{what} are limited to {max} bytes"),
    )
}

/// `POST uploads` with the file as the body: the blob reference and the
/// daemon-side path a `prompt.submit` attaches.
pub(crate) async fn upload(shared: &Shared, at: &Profile<'_>, req: &Req, body: Incoming) -> Reply {
    let max = shared.config.max_upload_bytes;
    store(at, req, body, max, "uploads (dashboard.max_upload_bytes)").await
}

/// `POST stage`: a webapp file, at most 16 MiB (and the upload cap).
pub(crate) async fn stage(shared: &Shared, at: &Profile<'_>, req: &Req, body: Incoming) -> Reply {
    let max = shared.config.max_upload_bytes.min(MAX_STAGED_BYTES);
    store(at, req, body, max, "staged files").await
}

async fn store(at: &Profile<'_>, req: &Req, body: Incoming, max: u64, what: &str) -> Reply {
    let declared = req
        .header(CONTENT_LENGTH.as_str())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max) {
        return too_large(max, what);
    }
    let Some(data) = read_body(body, usize::try_from(max).unwrap_or(usize::MAX)).await else {
        return too_large(max, what);
    };
    match at.store.put_blob(&data) {
        Ok(blob) => json_reply(
            201,
            &json!({"blob": blob.to_string(), "bytes": data.len(),
                    "path": at.store.blob_path(&blob)}),
        ),
        Err(err) => from_error(&err),
    }
}

/// `GET blobs/{sha256}`.
pub(crate) fn download(at: &Profile<'_>, sha: &str) -> Reply {
    let Some(blob) = BlobRef::parse(&format!("blob:sha256:{sha}")) else {
        return error(404, "not_found", "no such blob");
    };
    let referenced = at.store.blob_references(&blob).is_ok_and(|n| n > 0);
    if !referenced {
        return error(
            404,
            "not_found",
            "no session of this profile references that blob",
        );
    }
    match at.store.get_blob(&blob) {
        Ok(data) => {
            let mut reply = bytes(200, "application/octet-stream", data);
            reply
                .headers_mut()
                .insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
            reply
        }
        Err(err) => from_error(&err),
    }
}
