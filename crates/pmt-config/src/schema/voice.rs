//! The `[voice]` section (spec: voice-mode): push-to-talk voice
//! conversations on the CLI, TUI and desktop. Speech providers come from
//! `[stt]` and `[tts]`; this section only shapes the conversation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::KeyChord;

/// The `[voice]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct VoiceModeConfig {
    /// The key that starts a recording in voice mode, and stops the
    /// continuous loop when pressed during one.
    pub record_key: KeyChord,
    /// Play a start beep (880 Hz) and an end double beep (660 Hz) around
    /// each recording.
    pub beep_enabled: bool,
    /// The longest recording; it stops there.
    #[schemars(range(min = 5, max = 600))]
    pub max_recording_secs: u64,
    /// Input level (RMS, 0-32767) above which audio counts as speech.
    #[schemars(range(min = 1, max = 32767))]
    pub silence_threshold: u32,
    /// How long the level must stay above the threshold before speech is
    /// confirmed (dips shorter than 200 ms are tolerated).
    pub speech_confirm_ms: u64,
    /// Continuous quiet after confirmed speech that ends the recording.
    pub silence_duration_ms: u64,
    /// A recording with no confirmed speech by then ends without being
    /// transcribed.
    pub no_speech_timeout_secs: u64,
    /// Whole utterances that end the voice conversation instead of being
    /// sent (case and surrounding punctuation ignored); `[]` disables.
    pub stop_phrases: Vec<String>,
    /// Drop transcripts that are known silence hallucinations ("thanks
    /// for watching", one phrase repeated).
    pub hallucination_filter: bool,
    /// Turn spoken replies on whenever voice mode starts.
    pub auto_tts: bool,
}

impl Default for VoiceModeConfig {
    fn default() -> Self {
        Self {
            record_key: KeyChord("ctrl+b".to_owned()),
            beep_enabled: true,
            max_recording_secs: 120,
            silence_threshold: 200,
            speech_confirm_ms: 300,
            silence_duration_ms: 3_000,
            no_speech_timeout_secs: 15,
            stop_phrases: ["stop", "never mind", "goodbye", "cancel", "that's all"]
                .map(str::to_owned)
                .to_vec(),
            hallucination_filter: true,
            auto_tts: false,
        }
    }
}
