//! Chunked PCM speech (spec: voice-mode, Sentence-streamed spoken
//! replies: "streaming raw PCM for providers that offer chunked audio").
//! `openai` (and endpoints compatible with its speech API) and
//! `elevenlabs` stream raw 16-bit mono PCM at [`RATE`] while they
//! synthesize, so a spoken sentence starts playing before its audio is
//! complete; other providers synthesize whole files.

use reqwest::header::CONTENT_TYPE;
use serde_json::json;
use tokio::sync::mpsc;

use super::tts::{self, Speak};
use crate::MediaSession;
use crate::http::{status_error, transport};

/// Samples per second of streamed speech.
pub const RATE: u32 = 24_000;

/// Chunks buffered between the network and the player.
const BUFFERED: usize = 32;

/// Streamed speech: its rate and its chunks, in order (the stream ends
/// when the sender drops; a failure mid-stream ends it early).
pub type PcmStream = (u32, mpsc::Receiver<Vec<u8>>);

/// The providers that stream PCM.
#[must_use]
pub fn streams(provider: &str) -> bool {
    matches!(provider, "openai" | "elevenlabs")
}

impl MediaSession {
    /// `text` as streamed PCM when the session's provider streams;
    /// `None` when it does not (synthesize a file instead).
    ///
    /// # Errors
    ///
    /// The provider is not configured or refused the request.
    pub async fn speak_streaming(
        &self,
        text: &str,
        options: &Speak,
    ) -> Option<Result<PcmStream, String>> {
        let setup = self.setup();
        let voice = match tts::resolve(setup) {
            Ok(voice) => voice,
            Err(why) => return Some(Err(why)),
        };
        if voice.command.is_some() || voice.plugin.is_some() || !streams(&voice.provider) {
            return None;
        }
        let token = match voice.access.bearer() {
            Ok(token) => token.expose().to_owned(),
            Err(why) => return Some(Err(why)),
        };
        let request = if voice.provider == "elevenlabs" {
            let chosen = options.voice.as_deref().unwrap_or(&voice.voice);
            setup
                .client
                .post(format!(
                    "{}/text-to-speech/{chosen}/stream?output_format=pcm_{RATE}",
                    voice.base_url
                ))
                .header("xi-api-key", token)
                .header(CONTENT_TYPE, "application/json")
                .body(json!({"text": text, "model_id": voice.model}).to_string())
        } else {
            let mut body = json!({"model": voice.model, "input": text, "voice": voice.voice,
                "response_format": "pcm"});
            if let Some(speed) = options.speed.or(setup.config().tts.openai.speed) {
                body["speed"] = json!(speed);
            }
            setup
                .client
                .post(format!("{}/audio/speech", voice.base_url))
                .bearer_auth(token)
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string())
        };
        Some(open(request).await)
    }
}

/// Sends `request` and forwards its body chunk by chunk.
async fn open(request: reqwest::RequestBuilder) -> Result<PcmStream, String> {
    let mut response = request.send().await.map_err(|e| transport(&e))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default();
        return Err(status_error(status.as_u16(), &body));
    }
    let (chunks, received) = mpsc::channel(BUFFERED);
    pmt_core::spawn::spawn(async move {
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if chunks.send(chunk.to_vec()).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(error = %transport(&err), "streamed speech ended early");
                    return;
                }
            }
        }
    });
    Ok((RATE, received))
}
