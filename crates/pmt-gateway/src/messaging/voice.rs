//! Voice in and out (spec: platform-adapters, Voice input transcription;
//! Voice replies and streaming TTS). The profile's speech providers are
//! reached through [`Speech`] (the daemon wires the configured
//! speech-to-text and text-to-speech providers; web-and-media-tools). An
//! inbound voice or audio clip is transcribed and the transcript prepended
//! to the text (echoed back once when `gateway.voice.echo_transcripts`);
//! when transcription is off or fails, the agent gets a note naming the
//! cached file instead and the turn proceeds.

use std::path::{Path, PathBuf};

use pmt_config::schema::{VoiceConfig, VoiceReplyMode};

use super::adapter::AdapterFuture;

/// A profile's speech providers.
pub trait Speech: Send + Sync {
    /// The transcript of the audio file at `path`.
    fn transcribe<'a>(&'a self, path: &'a Path) -> AdapterFuture<'a, Result<String, String>>;
    /// Synthesizes `text` into an audio file in `out_dir`, returning its
    /// path.
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        out_dir: &'a Path,
    ) -> AdapterFuture<'a, Result<PathBuf, String>>;
}

/// The note the agent gets for a voice message it cannot read.
#[must_use]
pub fn untranscribed_note(path: &Path, duration_secs: Option<u64>) -> String {
    let duration = duration_secs.map_or_else(|| "?".to_owned(), |s| s.to_string());
    format!(
        "[The user sent a voice message: {} (duration: {duration}s)]",
        path.display()
    )
}

/// What a transcribed clip contributes to the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heard {
    /// Text prepended to the user's message.
    pub(crate) text: String,
    /// Echoed back to the chat, once.
    pub(crate) echo: Option<String>,
}

/// Transcribes the clip at `path` per `config`.
pub(crate) async fn hear(
    speech: Option<&dyn Speech>,
    config: &VoiceConfig,
    path: &Path,
    duration_secs: Option<u64>,
) -> Heard {
    let note = || Heard {
        text: untranscribed_note(path, duration_secs),
        echo: None,
    };
    let Some(speech) = speech.filter(|_| config.stt_enabled) else {
        return note();
    };
    match speech.transcribe(path).await {
        Ok(transcript) if !transcript.trim().is_empty() => Heard {
            echo: config
                .echo_transcripts
                .then(|| format!("Heard: \"{}\"", transcript.trim())),
            text: transcript.trim().to_owned(),
        },
        Ok(_) => note(),
        Err(err) => {
            tracing::warn!("voice message not transcribed: {err}");
            note()
        }
    }
}

/// Whether a reply is spoken: always, never, or when the user spoke.
#[must_use]
pub fn speaks(mode: VoiceReplyMode, user_spoke: bool) -> bool {
    match mode {
        VoiceReplyMode::Off => false,
        VoiceReplyMode::Mirror => user_spoke,
        VoiceReplyMode::Always => true,
    }
}

/// Splits streamed reply text into the sentences ready to speak and the
/// unfinished rest.
pub(crate) fn sentences(buffer: &str) -> (Vec<String>, String) {
    let mut done = Vec::new();
    let mut start = 0;
    for (at, c) in buffer.char_indices() {
        if matches!(c, '.' | '!' | '?' | '\n') {
            let end = at + c.len_utf8();
            let sentence = buffer[start..end].trim();
            if !sentence.is_empty() {
                done.push(sentence.to_owned());
            }
            start = end;
        }
    }
    (done, buffer[start..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_speaks_only_to_a_speaker() {
        assert!(speaks(VoiceReplyMode::Mirror, true));
        assert!(!speaks(VoiceReplyMode::Mirror, false));
        assert!(speaks(VoiceReplyMode::Always, false));
        assert!(!speaks(VoiceReplyMode::Off, true));
    }

    #[test]
    fn complete_sentences_are_spoken_as_they_arrive() {
        let (done, rest) = sentences("Hello there. How are");
        assert_eq!(done, ["Hello there."]);
        assert_eq!(rest, " How are");
    }
}
