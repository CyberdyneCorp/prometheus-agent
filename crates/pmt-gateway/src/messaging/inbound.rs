//! An admitted event's media, before its conversation sees it (spec:
//! platform-adapters, Inbound media limits and caching; Voice input
//! transcription): each attachment is downloaded into the owning
//! profile's media cache under the size cap (an oversize one is dropped
//! and the user told), and voice and audio clips become their transcript
//! (or a note naming the cached file).

use std::sync::Arc;

use pmt_config::ProfileScope;

use super::Shared;
use super::adapter::{Inbound, MediaKind};
use super::media::{self, Refused};
use super::voice;

/// Downloads `inbound`'s attachments through adapter `platform` of
/// `adapter_profile` and turns its clips into text; `true` when the user
/// sent a voice note.
pub(crate) async fn prepare(
    shared: &Arc<Shared>,
    adapter_profile: &str,
    platform: &str,
    scope: &ProfileScope,
    inbound: &mut Inbound,
) -> bool {
    if inbound.attachments.is_empty() {
        return false;
    }
    let Some(slot) = shared.adapters.ready(adapter_profile, platform) else {
        return false;
    };
    let config = &scope.config().gateway;
    let cache = media::cache_dir(scope.dir());
    let mut kept = Vec::new();
    for mut attachment in std::mem::take(&mut inbound.attachments) {
        match media::fetch(slot.adapter.as_ref(), &attachment, &config.media, &cache).await {
            Ok(path) => attachment.location = path.to_string_lossy().into_owned(),
            Err(Refused::Unsupported) => {}
            Err(refused) => {
                tracing::info!(platform, "attachment not cached: {refused:?}");
                super::admit::reply(
                    shared,
                    adapter_profile,
                    platform,
                    inbound,
                    &refused.notice(),
                )
                .await;
                continue;
            }
        }
        kept.push(attachment);
    }
    let speech = shared.agents.speech(scope.name().as_str());
    let mut voice_note = false;
    let mut heard = Vec::new();
    for attachment in &kept {
        if !matches!(attachment.kind, MediaKind::Voice | MediaKind::Audio) {
            continue;
        }
        voice_note |= attachment.kind == MediaKind::Voice;
        let clip = voice::hear(
            speech.as_deref(),
            &config.voice,
            std::path::Path::new(&attachment.location),
            attachment.duration_secs,
        )
        .await;
        if let Some(echo) = &clip.echo {
            super::admit::reply(shared, adapter_profile, platform, inbound, echo).await;
        }
        heard.push(clip.text);
    }
    kept.retain(|a| !matches!(a.kind, MediaKind::Voice | MediaKind::Audio));
    inbound.attachments = kept;
    if !heard.is_empty() {
        heard.extend(Some(inbound.text.clone()).filter(|t| !t.trim().is_empty()));
        inbound.text = heard.join("\n");
    }
    voice_note
}
