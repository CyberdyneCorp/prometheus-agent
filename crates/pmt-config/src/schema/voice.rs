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
#[allow(clippy::struct_excessive_bools)] // independent user switches
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
    /// Transcribe while the user speaks when the STT provider streams
    /// (partial text shown live); a failed stream falls back to
    /// transcribing the whole recording.
    pub stt_streaming: bool,
    /// Keep the microphone live while a reply generates and plays:
    /// speaking interrupts it.
    pub barge_in: bool,
    /// How far above the quiet room's level (calibrated at turn start)
    /// speech must rise to interrupt a reply.
    #[schemars(range(min = 1.0, max = 10.0))]
    pub barge_in_threshold_multiplier: f64,
    /// Keep recordings under `cache/voice/` after transcription (they
    /// then expire after `gateway.media.cache_max_age_secs`); off deletes
    /// each as soon as it is transcribed.
    pub keep_recordings: bool,
    /// A desktop attached to a remote daemon talks to cloud speech
    /// providers itself, so audio never crosses the daemon link.
    pub client_direct: bool,
    /// `chained` (record, transcribe, turn, speak) or `realtime` (one
    /// full-duplex realtime voice session on the desktop that hands work
    /// to the agent).
    pub chat_mode: VoiceChatMode,
    /// The realtime voice session of `chat_mode = "realtime"`.
    pub realtime: RealtimeVoiceConfig,
}

/// `voice.chat_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceChatMode {
    /// Record, transcribe, run the turn, speak the reply.
    #[default]
    Chained,
    /// A full-duplex realtime voice model (desktop only).
    Realtime,
}

/// `[voice.realtime]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RealtimeVoiceConfig {
    /// The realtime provider (`openai`; its key stays on the daemon).
    pub provider: String,
    /// The realtime model.
    pub model: String,
    /// The voice it speaks with.
    pub voice: String,
    /// The provider's API base URL (the daemon creates the call there with
    /// the profile's key).
    pub base_url: String,
    /// Extra instructions for the voice layer (the agent keeps its own
    /// system prompt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl Default for RealtimeVoiceConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_owned(),
            model: "gpt-realtime".to_owned(),
            voice: "marin".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            instructions: None,
        }
    }
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
            stt_streaming: false,
            barge_in: true,
            barge_in_threshold_multiplier: 3.0,
            keep_recordings: false,
            client_direct: true,
            chat_mode: VoiceChatMode::Chained,
            realtime: RealtimeVoiceConfig::default(),
        }
    }
}
