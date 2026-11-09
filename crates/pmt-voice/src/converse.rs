//! The continuous voice conversation (spec: voice-mode, Push-to-talk
//! recording and continuous loop; Spoken and typed stop phrases;
//! Transcription hallucination filter).
//!
//! The record key starts [`converse`]: record, transcribe, submit the
//! transcript as a user message, speak the reply when spoken replies are
//! on, and record again without a key press. The loop ends when the
//! record key is pressed during a recording, when a stop phrase is said
//! (or typed while the conversation is active), or after
//! [`QUIET_RECORDINGS`] consecutive recordings without speech; a
//! transcript the hallucination filter drops counts as no speech.

use std::future::Future;
use std::sync::Arc;

use pmt_config::schema::VoiceModeConfig;

use crate::device::{Microphone, Player, Synth, Transcriber};
use crate::filter;
use crate::record::{self, Meter};
use crate::speaker::Mouth;

/// Consecutive recordings without speech that end the conversation.
pub const QUIET_RECORDINGS: u32 = 3;

/// What the conversation runs with.
#[derive(Clone)]
pub struct Kit {
    /// The input device.
    pub mic: Arc<dyn Microphone>,
    /// Speech-to-text.
    pub ears: Arc<dyn Transcriber>,
    /// Plays the cues and spoken replies.
    pub player: Arc<dyn Player>,
    /// Text-to-speech while spoken replies are on.
    pub voice: Option<Arc<dyn Synth>>,
    /// Shows the input level while recording.
    pub meter: Meter,
    /// `[voice]`.
    pub config: VoiceModeConfig,
}

/// What the user did while a recording ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interjection {
    /// Pressed the record key.
    RecordKey,
    /// Typed a line.
    Line(String),
}

/// The surface the conversation runs on.
pub trait Host {
    /// Runs one user turn of `text`; its reply's text deltas go to
    /// `mouth` when spoken replies are on.
    fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>) -> impl Future<Output = ()>;
    /// Waits for the user to press the record key or type a line.
    fn interjection(&mut self) -> impl Future<Output = Interjection>;
    /// Handles a typed line that is not a stop phrase as typed input;
    /// `false` when the conversation must end (voice mode was turned off).
    fn typed(&mut self, line: String) -> impl Future<Output = bool>;
    /// Shows a transcript about to be sent.
    fn heard(&mut self, transcript: &str);
}

/// Why a conversation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The record key was pressed during a recording.
    RecordKey,
    /// A stop phrase was said or typed.
    StopPhrase,
    /// [`QUIET_RECORDINGS`] recordings in a row had no speech.
    Silence,
    /// A typed command turned voice mode off.
    VoiceOff,
    /// Recording or transcription failed.
    Failed(String),
}

impl Ended {
    /// The line the surface prints.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::RecordKey | Self::StopPhrase | Self::VoiceOff => {
                "Voice conversation ended.".to_owned()
            }
            Self::Silence => {
                format!("Listening stopped: no speech in {QUIET_RECORDINGS} recordings in a row.")
            }
            Self::Failed(why) => format!("Voice conversation ended: {why}"),
        }
    }
}

/// What one recording amounted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// A request.
    Speech(String),
    /// A stop phrase.
    Stop,
    /// Nothing (no speech, or a dropped hallucination).
    Nothing,
}

/// Records and transcribes one utterance and says what it is.
///
/// # Errors
///
/// The microphone or the transcriber failed.
pub async fn listen(kit: &Kit) -> Result<Heard, String> {
    let recording = record::record(
        kit.mic.as_ref(),
        kit.player.as_ref(),
        &kit.config,
        &kit.meter,
    )
    .await?;
    if !recording.has_speech() {
        return Ok(Heard::Nothing);
    }
    let transcript = kit.ears.transcribe(&recording.samples).await?;
    Ok(judge(&transcript, &kit.config))
}

/// What a transcript means under `[voice]`.
#[must_use]
pub fn judge(transcript: &str, config: &VoiceModeConfig) -> Heard {
    let text = transcript.trim();
    if text.is_empty() || (config.hallucination_filter && filter::is_hallucination(text)) {
        return Heard::Nothing;
    }
    if filter::is_stop_phrase(text, &config.stop_phrases) {
        return Heard::Stop;
    }
    Heard::Speech(text.to_owned())
}

/// Runs the continuous conversation until it ends.
pub async fn converse<H: Host>(host: &mut H, kit: &Kit) -> Ended {
    let mut quiet = 0;
    loop {
        // Biased: a recording always starts (and cues) before a waiting
        // key press or line stops it, so the order of events is fixed.
        let heard = tokio::select! {
            biased;
            heard = listen(kit) => heard,
            interjection = host.interjection() => match interjection {
                Interjection::RecordKey => return Ended::RecordKey,
                Interjection::Line(line) => {
                    if filter::is_stop_phrase(&line, &kit.config.stop_phrases) {
                        return Ended::StopPhrase;
                    }
                    if !host.typed(line).await {
                        return Ended::VoiceOff;
                    }
                    continue;
                }
            },
        };
        match heard {
            Ok(Heard::Speech(text)) => {
                quiet = 0;
                host.heard(&text);
                let mouth = kit
                    .voice
                    .as_ref()
                    .map(|voice| Mouth::start(Arc::clone(voice), Arc::clone(&kit.player)));
                host.turn(text, mouth.clone()).await;
                if let Some(mouth) = mouth {
                    mouth.finish().await;
                }
            }
            Ok(Heard::Stop) => return Ended::StopPhrase,
            Ok(Heard::Nothing) => {
                quiet += 1;
                if quiet >= QUIET_RECORDINGS {
                    return Ended::Silence;
                }
            }
            Err(why) => return Ended::Failed(why),
        }
    }
}
