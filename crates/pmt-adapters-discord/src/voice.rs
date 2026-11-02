//! Voice messages: Discord shows an audio attachment as a voice note only
//! when the message carries the `IS_VOICE_MESSAGE` flag, no text, exactly
//! one file, and the clip's length and waveform in the attachment's
//! metadata.

use std::path::Path;

use pmt_gateway::messaging::SendError;
use pmt_gateway::messaging::multipart::Form;
use serde_json::json;

use crate::api::Body;

/// The `IS_VOICE_MESSAGE` message flag.
pub(crate) const FLAG: u64 = 1 << 13;

/// A flat waveform (32 mid-level samples, base64): the adapter does not
/// decode audio, and Discord only draws it.
const WAVEFORM: &str = "gICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIA=";

/// The length of an Ogg Opus clip in seconds: the last page's granule
/// position counts 48 kHz samples. One second when the file is not Ogg.
pub(crate) fn duration_secs(bytes: &[u8]) -> f64 {
    let last_page = bytes.windows(4).rposition(|w| w == b"OggS");
    let granule = last_page
        .and_then(|at| bytes.get(at + 6..at + 14))
        .and_then(|raw| <[u8; 8]>::try_from(raw).ok())
        .map(u64::from_le_bytes)
        .filter(|samples| *samples > 0 && *samples != u64::MAX);
    #[allow(clippy::cast_precision_loss)] // clip lengths are far below 2^52 samples
    granule.map_or(1.0, |samples| samples as f64 / 48_000.0)
}

/// The voice message carrying the clip at `path` into a channel.
pub(crate) fn message(path: &Path) -> Result<Body, SendError> {
    let bytes = std::fs::read(path)
        .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
    let name = path.file_name().map_or_else(
        || "voice-message.ogg".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let payload = json!({
        "flags": FLAG,
        "attachments": [{
            "id": 0, "filename": name,
            "duration_secs": duration_secs(&bytes), "waveform": WAVEFORM,
        }],
    });
    let mut form = Form::new();
    form.text("payload_json", &payload.to_string());
    form.file("files[0]", &name, &bytes);
    let kind = form.content_type();
    Ok(Body::Form(kind, form.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ogg_clip_is_as_long_as_its_last_granule() {
        let mut page = b"OggS\0\x04".to_vec();
        page.extend_from_slice(&(96_000_u64).to_le_bytes());
        assert!((duration_secs(&page) - 2.0).abs() < f64::EPSILON);
        assert!((duration_secs(b"not ogg") - 1.0).abs() < f64::EPSILON);
    }
}
