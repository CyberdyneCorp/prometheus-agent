//! Live streaming transcription (spec: voice-mode, Live streaming
//! transcription). While `voice.stt_streaming` is on and the profile's
//! provider streams, each recorded frame also goes to a realtime
//! transcription session whose partial text the surface shows as the user
//! speaks; its final transcript is ready when the recording ends. The
//! recording itself is always kept, so a session that cannot open or
//! breaks mid-utterance costs nothing: the whole recording is transcribed
//! the ordinary way and the utterance is never lost.

use std::sync::Arc;

use crate::device::{BoxFuture, Transcriber};

/// Shows the transcript so far.
pub type Partial = Arc<dyn Fn(&str) + Send + Sync>;

/// A provider that transcribes while the user speaks.
pub trait StreamingTranscriber: Send + Sync {
    /// Opens a session; `partial` gets the text heard so far.
    fn open(&self, partial: Partial) -> BoxFuture<'_, Result<Box<dyn SttStream>, String>>;
}

/// One open streaming session.
pub trait SttStream: Send {
    /// Sends more audio (16 kHz mono); an error means the session broke.
    ///
    /// # Errors
    ///
    /// The session is gone.
    fn push(&mut self, samples: &[i16]) -> Result<(), String>;
    /// No more audio: the final transcript.
    fn finish(self: Box<Self>) -> BoxFuture<'static, Result<String, String>>;
}

/// A streaming session that may already have broken.
pub struct Live {
    stream: Option<Box<dyn SttStream>>,
}

impl Live {
    /// Opens a session on `streaming`, if any; a session that cannot open
    /// leaves the recording to the file path.
    pub async fn open(streaming: Option<&Arc<dyn StreamingTranscriber>>, partial: Partial) -> Self {
        let stream = match streaming {
            Some(streaming) => match streaming.open(partial).await {
                Ok(stream) => Some(stream),
                Err(why) => {
                    tracing::warn!(%why, "streaming transcription did not open; the recording will be transcribed whole");
                    None
                }
            },
            None => None,
        };
        Self { stream }
    }

    /// Sends a frame; a broken session is dropped.
    pub fn push(&mut self, samples: &[i16]) {
        if let Some(stream) = &mut self.stream
            && let Err(why) = stream.push(samples)
        {
            tracing::warn!(%why, "streaming transcription broke; the recording will be transcribed whole");
            self.stream = None;
        }
    }

    /// The transcript of `samples`: the session's when it lasted, else the
    /// whole recording through `file`.
    ///
    /// # Errors
    ///
    /// The fallback transcription failed.
    pub async fn finish(self, file: &dyn Transcriber, samples: &[i16]) -> Result<String, String> {
        if let Some(stream) = self.stream {
            match stream.finish().await {
                Ok(text) => return Ok(text),
                Err(why) => {
                    tracing::warn!(%why, "streaming transcription failed at the end; transcribing the recording whole");
                }
            }
        }
        file.transcribe(samples).await
    }

    /// Whether a session is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }
}
