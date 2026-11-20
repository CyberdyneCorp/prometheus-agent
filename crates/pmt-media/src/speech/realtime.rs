//! Live streaming transcription (spec: voice-mode, Live streaming
//! transcription): providers that declare a realtime transcription
//! socket ([`STREAMING`]) get the recording frame by frame while the user
//! speaks and send partial text back. `OpenAI`'s realtime transcription
//! session takes 24 kHz PCM16, so the 16 kHz capture is resampled on the
//! way out. A session that breaks is reported, never retried: the caller
//! keeps the recording and transcribes it whole.

use std::sync::Arc;

use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::schema::NetworkConfig;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::stt::builtin_listener;
use crate::MediaSession;

/// The providers with a realtime transcription socket.
pub const STREAMING: &[&str] = &["openai"];

/// The realtime transcription model when `stt.openai.model` is not one
/// (`whisper-1` has no streaming deltas).
pub const DEFAULT_MODEL: &str = "gpt-4o-transcribe";

/// Shows the transcript so far.
pub type Partial = Arc<dyn Fn(&str) + Send + Sync>;

/// A provider's realtime transcription, ready to open.
#[derive(Debug, Clone)]
pub struct StreamingStt {
    /// The provider.
    pub provider: String,
    url: String,
    token: pmt_core::secret::SecretString,
    model: String,
    language: Option<String>,
    network: NetworkConfig,
}

impl MediaSession {
    /// The streaming transcription of the provider a transcription would
    /// use now, when it declares one.
    #[must_use]
    pub fn stt_streaming(&self) -> Option<StreamingStt> {
        let setup = self.setup();
        let listener = builtin_listener(setup)?;
        if !STREAMING.contains(&listener.provider.as_str()) {
            return None;
        }
        let token = listener.access.token.clone()?;
        let model = if listener.model.starts_with("gpt-") {
            listener.model.clone()
        } else {
            DEFAULT_MODEL.to_owned()
        };
        Some(StreamingStt {
            url: socket_url(&listener.base_url),
            provider: listener.provider,
            token,
            model,
            language: listener.language,
            network: setup.config().network.clone(),
        })
    }
}

/// `https://host/v1` -> `wss://host/v1/realtime?intent=transcription`.
fn socket_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let socket = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_owned());
    format!("{socket}/realtime?intent=transcription")
}

/// 16 kHz to 24 kHz by linear interpolation.
#[must_use]
pub fn upsample(samples: &[i16]) -> Vec<i16> {
    let Some(&last) = samples.last() else {
        return Vec::new();
    };
    let out = samples.len() * 3 / 2;
    (0..out)
        .map(|n| {
            let at = n * 2;
            // The position between two input samples, in thirds.
            let (whole, part) = (at / 3, [0.0, 1.0, 2.0][at % 3]);
            let a = f64::from(samples[whole]);
            let b = f64::from(samples.get(whole + 1).copied().unwrap_or(last));
            #[allow(clippy::cast_possible_truncation)]
            let sample = (a + (b - a) * part / 3.0).round() as i16;
            sample
        })
        .collect()
}

fn pcm_b64(samples: &[i16]) -> String {
    let bytes: Vec<u8> = upsample(samples)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// An open session: frames go out as they come, the final transcript
/// comes back after [`SttSession::finish`].
pub struct SttSession {
    frames: Option<mpsc::UnboundedSender<Vec<i16>>>,
    done: oneshot::Receiver<Result<String, String>>,
}

impl StreamingStt {
    /// Opens a session; `partial` gets the text heard so far.
    ///
    /// # Errors
    ///
    /// The socket did not open or refused the session.
    pub async fn open(&self, partial: Partial) -> Result<SttSession, String> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("invalid realtime URL: {e}"))?;
        let bearer = format!("Bearer {}", self.token.expose());
        request.headers_mut().insert(
            "Authorization",
            bearer.parse().map_err(|_| "unusable API key")?,
        );
        let mut socket =
            super::edge::dial(request, &self.network, "realtime transcription").await?;
        let mut transcription = json!({"model": self.model});
        if let Some(language) = &self.language {
            transcription["language"] = json!(language);
        }
        let update = json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {"input": {
                    "format": {"type": "audio/pcm", "rate": 24_000},
                    "transcription": transcription,
                    "turn_detection": null,
                }},
            },
        });
        socket
            .send(Message::text(update.to_string()))
            .await
            .map_err(|e| format!("realtime transcription: {e}"))?;
        let (frames, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let (finished, done) = oneshot::channel();
        pmt_core::spawn::spawn(async move {
            let result = run(&mut socket, &mut rx, &partial).await;
            let _ = socket.close(None).await;
            let _ = finished.send(result);
        });
        Ok(SttSession {
            frames: Some(frames),
            done,
        })
    }
}

/// Sends the frames, then commits and waits for the final transcript.
async fn run<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    frames: &mut mpsc::UnboundedReceiver<Vec<i16>>,
    partial: &Partial,
) -> Result<String, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut heard = String::new();
    let mut sending = true;
    loop {
        tokio::select! {
            frame = frames.recv(), if sending => {
                let message = if let Some(samples) = frame {
                    json!({"type": "input_audio_buffer.append", "audio": pcm_b64(&samples)})
                } else {
                    sending = false;
                    json!({"type": "input_audio_buffer.commit"})
                };
                socket
                    .send(Message::text(message.to_string()))
                    .await
                    .map_err(|e| format!("realtime transcription: {e}"))?;
            }
            message = socket.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => {
                        return Err("the realtime transcription socket closed".to_owned());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(format!("realtime transcription: {e}")),
                };
                if let Some(done) = event(&text, &mut heard, partial)? {
                    return Ok(done);
                }
            }
        }
    }
}

/// Handles one server event: `Some` with the final transcript.
fn event(text: &str, heard: &mut String, partial: &Partial) -> Result<Option<String>, String> {
    let event: Value = serde_json::from_str(text).unwrap_or_default();
    match event["type"].as_str().unwrap_or_default() {
        "conversation.item.input_audio_transcription.delta" => {
            heard.push_str(event["delta"].as_str().unwrap_or_default());
            partial(heard.trim());
            Ok(None)
        }
        "conversation.item.input_audio_transcription.completed" => Ok(Some(
            event["transcript"]
                .as_str()
                .unwrap_or(heard)
                .trim()
                .to_owned(),
        )),
        "conversation.item.input_audio_transcription.failed" | "error" => {
            let why = event["error"]["message"]
                .as_str()
                .unwrap_or("the provider reported an error");
            Err(format!("realtime transcription: {why}"))
        }
        _ => Ok(None),
    }
}

impl SttSession {
    /// Sends more audio (16 kHz mono).
    ///
    /// # Errors
    ///
    /// The session has ended (the socket closed or failed).
    pub fn push(&mut self, samples: &[i16]) -> Result<(), String> {
        let frames = self
            .frames
            .as_ref()
            .ok_or("the realtime transcription was finished")?;
        frames
            .send(samples.to_vec())
            .map_err(|_| "the realtime transcription socket closed".to_owned())
    }

    /// No more audio: the final transcript.
    ///
    /// # Errors
    ///
    /// The session failed before the transcript came.
    pub async fn finish(mut self) -> Result<String, String> {
        self.frames.take();
        self.done
            .await
            .map_err(|_| "the realtime transcription stopped".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_url_follows_the_base_url() {
        assert_eq!(
            socket_url("https://api.openai.com/v1"),
            "wss://api.openai.com/v1/realtime?intent=transcription"
        );
        assert_eq!(
            socket_url("http://127.0.0.1:9/v1/"),
            "ws://127.0.0.1:9/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn upsampling_keeps_the_waveform() {
        assert_eq!(upsample(&[0, 300, 600, 900]), [0, 200, 400, 600, 800, 900]);
        assert!(upsample(&[]).is_empty());
    }
}
