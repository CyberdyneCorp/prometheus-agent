//! How a long text reaches a platform (spec: platform-adapters,
//! Fence-aware message splitting; Oversized output handling): split at
//! the descriptor's limit in its unit (whole when the adapter splits
//! itself); more than `gateway.delivery.max_parts` parts become the first
//! part plus a final `... [truncated, full output attached]` part carrying
//! the full text as a document, or, where the platform sends no
//! documents, a note naming the blob that holds it.

use pmt_store::gateway::outbox::OutboundAttachment;

use super::Shared;
use super::adapter::MediaKind;
use super::descriptor::Descriptor;
use super::{media, split};

/// The final part of a text too long for the platform.
pub const TRUNCATED: &str = "... [truncated, full output attached]";

/// What a text belongs to (names the saved full text).
pub(crate) struct Owner<'a> {
    /// The session, for the `sessions show` hint.
    pub(crate) session: Option<&'a str>,
    /// A unique key (the turn id, the send's key).
    pub(crate) key: &'a str,
}

/// The parts of `text` for `descriptor`'s platform, with the attachments
/// each carries.
pub(crate) fn parts(
    shared: &Shared,
    profile: &str,
    descriptor: &Descriptor,
    text: &str,
    owner: &Owner<'_>,
) -> Vec<(String, Vec<OutboundAttachment>)> {
    if descriptor.splits_long_messages {
        return vec![(text.to_owned(), Vec::new())];
    }
    let parts = split::split(text, descriptor.max_message_length, descriptor.length_unit);
    let max_parts = shared
        .agents
        .scope(profile)
        .map_or(10, |s| s.config().gateway.delivery.max_parts as usize);
    if parts.len() <= max_parts {
        return parts.into_iter().map(|p| (p, Vec::new())).collect();
    }
    let first = parts.into_iter().next().unwrap_or_default();
    let last = match full_text(shared, profile, descriptor, text, owner) {
        Full::Attached(file) => (TRUNCATED.to_owned(), vec![file]),
        Full::Saved(note) => (format!("... [truncated] {note}"), Vec::new()),
    };
    vec![(first, Vec::new()), last]
}

/// Where an oversized text's full version went.
enum Full {
    /// Attached as a document.
    Attached(OutboundAttachment),
    /// In the blob store (or nowhere): the note says.
    Saved(String),
}

fn show_hint(owner: &Owner<'_>) -> String {
    owner.session.map_or_else(String::new, |session| {
        format!("; run `prometheus sessions show {session}`")
    })
}

fn full_text(
    shared: &Shared,
    profile: &str,
    descriptor: &Descriptor,
    text: &str,
    owner: &Owner<'_>,
) -> Full {
    let scope = shared.agents.scope(profile);
    if descriptor.sends(MediaKind::Document)
        && let Some(dir) = scope.as_ref().map(|s| media::cache_dir(s.dir()))
    {
        let path = dir.join(format!("full-{}.txt", sanitize(owner.key)));
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&path, text).is_ok() {
            let mut file = OutboundAttachment::new(&path.to_string_lossy());
            file.as_document = true;
            return Full::Attached(file);
        }
    }
    let saved = shared.store(profile).map(|store| {
        let blob = store.put_blob(text.as_bytes())?;
        store.reference_blob(&blob, "gateway_reply", owner.key)?;
        Ok::<_, pmt_core::error::Error>(blob)
    });
    match saved {
        Some(Ok(blob)) => Full::Saved(format!("full output saved as {blob}{}", show_hint(owner))),
        Some(Err(err)) => {
            tracing::warn!("oversized text not saved: {err}");
            Full::Saved(format!("full output not saved{}", show_hint(owner)))
        }
        None => Full::Saved(format!("full output not saved{}", show_hint(owner))),
    }
}

/// A key usable in a file name.
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
