//! Voice mode's daemon methods (spec: voice-mode, Wake capture location
//! for remote clients; Wake and voice diagnostics): a client arms the
//! profile's wake listener on the daemon, with the daemon's microphone or
//! its own (`client_capture`: it then streams 16 kHz mono PCM through
//! `wake.feed`), and hears `wake.detected` when the phrase is spoken.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method};

/// `wake.arm` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeArmParams {
    /// The profile whose wake word listens.
    pub profile: String,
    /// The client sends the audio (`wake.feed`) instead of the daemon
    /// opening its own microphone (used when `wake.capture = "auto"`).
    #[serde(default)]
    pub client_capture: bool,
}

/// `wake.arm` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeArmed {
    /// The engine listening.
    pub engine: String,
    /// The phrase it listens for.
    pub phrase: String,
    /// `local` (the daemon's microphone) or `client` (`wake.feed`).
    pub capture: String,
}

/// `wake.feed` params: audio from the client's microphone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeFeedParams {
    /// The profile whose listener takes the audio.
    pub profile: String,
    /// 16 kHz mono 16-bit little-endian PCM, base64.
    pub pcm: String,
}

/// A profile's wake listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeProfileParams {
    /// The profile.
    pub profile: String,
}

/// `wake.status` result (and the empty answer of `wake.feed` and
/// `wake.disarm`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeState {
    /// The daemon's listener for the profile is armed.
    pub armed: bool,
    /// The `/wake status` report (phrase, engine, owner, capture, input
    /// device, audio host API, what is missing, a silent microphone).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub report: String,
    /// Why the listener stopped or could not arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `wake.detected` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeDetected {
    /// The profile the phrase wakes (another profile's under profile
    /// routing: the desktop switches to it).
    pub profile: String,
    /// The phrase heard.
    pub phrase: String,
}

method!(/// `wake.arm`: arms the profile's wake listener on the daemon (the
    /// desktop's ear): it needs an STT and a TTS provider, takes the
    /// machine's wake lock for the desktop and, with client capture,
    /// listens to `wake.feed` audio only.
    WakeArm = "wake.arm" (WakeArmParams) -> WakeArmed);
method!(/// `wake.feed`: audio for a client-capture listener; never sent to
    /// a third party.
    WakeFeed = "wake.feed" (WakeFeedParams) -> WakeState);
method!(/// `wake.disarm`: stops the profile's daemon listener and frees the
    /// wake lock.
    WakeDisarm = "wake.disarm" (WakeProfileParams) -> WakeState);
method!(/// `wake.status`: the daemon listener's state and report.
    WakeStatus = "wake.status" (WakeProfileParams) -> WakeState);
event!(/// `wake.detected`: the wake phrase was heard; the desktop records
    /// the request on its own microphone.
    WakeDetectedEvent = "wake.detected" (WakeDetected));

/// A profile's voice settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceProfileParams {
    /// The profile.
    pub profile: String,
}

/// How a client reaches one speech provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientSpeech {
    /// `direct` (the client calls the provider), `relay` (through the
    /// daemon: `voice.transcribe`, `voice.speak`) or `none`.
    pub path: String,
    /// The provider id.
    pub provider: String,
    /// Its model.
    pub model: String,
    /// The API base URL (direct only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// The credential (direct only): kept in memory for the voice session
    /// only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The voice (speech).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub voice: String,
    /// The spoken language (transcription).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Why the relay path (or none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `voice.client_config` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceClientConfig {
    /// `voice.client_direct`.
    pub client_direct: bool,
    /// `voice.chat_mode` (`chained` or `realtime`).
    pub chat_mode: String,
    /// Speech-to-text.
    pub stt: ClientSpeech,
    /// Text-to-speech.
    pub tts: ClientSpeech,
}

/// `voice.transcribe` params: a recording relayed through the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceTranscribeParams {
    /// The profile whose speech-to-text provider transcribes.
    pub profile: String,
    /// The session the words go to (metered calls are billed to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The recording (WAV, Ogg, MP3 or `WebM`), base64.
    pub audio: String,
}

/// `voice.transcribe` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceTranscript {
    /// The text.
    pub text: String,
    /// The provider that transcribed it.
    pub provider: String,
}

method!(/// `voice.client_config`: how the client reaches the profile's
    /// speech providers: directly (with the credential, for cloud
    /// providers) or through the daemon (host-only or signed-in
    /// providers, or `voice.client_direct = false`).
    VoiceClientConfigGet = "voice.client_config" (VoiceProfileParams) -> VoiceClientConfig);
method!(/// `voice.transcribe`: transcribes a recording with the profile's
    /// speech-to-text provider on the daemon host (the relay path).
    VoiceTranscribe = "voice.transcribe" (VoiceTranscribeParams) -> VoiceTranscript);
