//! Media tool sections (spec: web-and-media-tools): `[x_search]`,
//! `[image_gen]`, `[tts]` and `[stt]`. Each category's `provider` is the
//! single selection key the subscription tool gateway routing reads
//! (model-providers, Subscription tool gateway routing); unset lets the
//! routing detect one. Per-provider defaults that the spec names live with
//! the providers (a partially written provider table keeps them).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The `[x_search]` section (the `x_search` tool, toolset `x_search`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct XSearchConfig {
    /// The xAI model answering X searches.
    pub model: String,
    /// Reasoning effort sent with each search (unset: the model's own).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Bound on one search request.
    #[schemars(range(min = 30))]
    pub timeout_secs: u64,
    /// Retries on 5xx, read timeouts and connection errors (backoff of
    /// 1.5 s per attempt, capped at 5 s).
    #[schemars(range(max = 10))]
    pub retries: u32,
}

impl Default for XSearchConfig {
    fn default() -> Self {
        Self {
            model: "grok-4.5".to_owned(),
            reasoning_effort: None,
            timeout_secs: 180,
            retries: 2,
        }
    }
}

/// The `[image_gen]` section (the `image_generate` tool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ImageGenConfig {
    /// The provider generating images: `fal` (default), `openai`,
    /// `openai-codex`, `openrouter`, `xai`, `krea`, `meta-ai`,
    /// `deepinfra`, `nous` (the subscription tool gateway) or a plugin's;
    /// unset picks one by the keys present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model (unset: the provider's default). The agent never picks
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Generations of one tool-call batch running at once (at most
    /// `agent.tool_concurrency`).
    #[schemars(range(min = 1))]
    pub max_parallel_requests: u32,
    /// The `openai` provider's endpoint and credential.
    pub openai: ImageOpenAiConfig,
}

impl Default for ImageGenConfig {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            max_parallel_requests: 4,
            openai: ImageOpenAiConfig::default(),
        }
    }
}

/// `[image_gen.openai]`: the `openai` image provider's endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImageOpenAiConfig {
    /// An OpenAI-compatible endpoint (`.../v1`); unset is OpenAI's API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The secret holding the key (default `OPENAI_API_KEY`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_secret: Option<SecretName>,
    /// A configured custom provider (`[providers.<id>]`) whose endpoint
    /// and key are used instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model on this endpoint (wins over `image_gen.model`); an id
    /// outside the catalog is sent verbatim, without `quality`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The `[tts]` section (the `text_to_speech` tool and spoken replies).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct TtsConfig {
    /// The speech provider: `edge` (default, keyless), `elevenlabs`,
    /// `openai`, `gemini`, `xai`, `mistral`, `minimax`, `deepinfra`,
    /// `nous`, the local engines `piper`, `kittentts`, `neutts`, a
    /// `[tts.providers.<name>]` command or a plugin's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Seconds a local engine stays loaded after the last surface released
    /// it (`0` unloads at once).
    pub keep_warm_secs: u64,
    /// Microsoft Edge read-aloud voices (keyless).
    pub edge: EdgeTtsConfig,
    /// `ElevenLabs` (`ELEVENLABS_API_KEY`).
    pub elevenlabs: ElevenLabsTtsConfig,
    /// `OpenAI` speech (`OPENAI_API_KEY`).
    pub openai: OpenAiTtsConfig,
    /// Gemini speech (`GEMINI_API_KEY`).
    pub gemini: GeminiTtsConfig,
    /// xAI speech (`XAI_API_KEY`).
    pub xai: XaiTtsConfig,
    /// Mistral Voxtral speech (`MISTRAL_API_KEY`; default model
    /// `voxtral-mini-tts-2603`).
    pub mistral: VoiceModelConfig,
    /// `MiniMax` speech (`MINIMAX_API_KEY`; default model `speech-02-hd`).
    pub minimax: VoiceModelConfig,
    /// `DeepInfra` speech (`DEEPINFRA_API_KEY`).
    pub deepinfra: VoiceModelConfig,
    /// The subscription tool gateway's speech.
    pub nous: VoiceModelConfig,
    /// Local Piper (default voice `en_US-lessac-medium`, downloaded on
    /// first use).
    pub piper: VoiceModelConfig,
    /// Local `KittenTTS`.
    pub kittentts: VoiceModelConfig,
    /// Local `NeuTTS`.
    pub neutts: VoiceModelConfig,
    /// User-declared command providers, selectable by name (a built-in
    /// name always wins).
    #[schemars(extend("x-apply" = "next_session"))]
    pub providers: BTreeMap<String, CommandTtsConfig>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            provider: None,
            keep_warm_secs: 60,
            edge: EdgeTtsConfig::default(),
            elevenlabs: ElevenLabsTtsConfig::default(),
            openai: OpenAiTtsConfig::default(),
            gemini: GeminiTtsConfig::default(),
            xai: XaiTtsConfig::default(),
            mistral: VoiceModelConfig::default(),
            minimax: VoiceModelConfig::default(),
            deepinfra: VoiceModelConfig::default(),
            nous: VoiceModelConfig::default(),
            piper: VoiceModelConfig::default(),
            kittentts: VoiceModelConfig::default(),
            neutts: VoiceModelConfig::default(),
            providers: BTreeMap::new(),
        }
    }
}

/// `[tts.edge]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeTtsConfig {
    /// The voice.
    pub voice: String,
    /// Characters per request (a positive integer; default 5,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

impl Default for EdgeTtsConfig {
    fn default() -> Self {
        Self {
            voice: "en-US-AriaNeural".to_owned(),
            max_text_length: None,
        }
    }
}

/// `[tts.elevenlabs]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ElevenLabsTtsConfig {
    /// The voice (the tool's `voice` argument may pick another).
    pub voice_id: String,
    /// The model; it decides the per-request cap.
    pub model_id: String,
    /// Characters per request (a positive integer; default by model).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

impl Default for ElevenLabsTtsConfig {
    fn default() -> Self {
        Self {
            voice_id: "pNInz6obpgDQGcFmaJgB".to_owned(),
            model_id: "eleven_multilingual_v2".to_owned(),
            max_text_length: None,
        }
    }
}

/// `[tts.openai]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiTtsConfig {
    /// The model.
    pub model: String,
    /// The voice.
    pub voice: String,
    /// An OpenAI-compatible endpoint (`.../v1`); unset is OpenAI's API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Confirms the operator may clone the configured custom voice.
    pub consent_attestation: bool,
    /// Speaking speed (0.25..4.0; the tool's `speed` argument wins).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.25, max = 4.0))]
    pub speed: Option<f64>,
    /// Characters per request (a positive integer; default 4,096).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

impl Default for OpenAiTtsConfig {
    fn default() -> Self {
        Self {
            model: "gpt-4o-mini-tts".to_owned(),
            voice: "alloy".to_owned(),
            base_url: None,
            consent_attestation: false,
            speed: None,
            max_text_length: None,
        }
    }
}

/// `[tts.gemini]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GeminiTtsConfig {
    /// The model.
    pub model: String,
    /// The prebuilt voice.
    pub voice: String,
    /// A prompt file framing the text; `{transcript}` marks where it goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona_prompt_file: Option<String>,
    /// Keep inline audio tags (`[laughs]`) as performance directions.
    pub audio_tags: bool,
    /// Characters per request (a positive integer; default 32,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

impl Default for GeminiTtsConfig {
    fn default() -> Self {
        Self {
            model: "gemini-2.5-flash-preview-tts".to_owned(),
            voice: "Kore".to_owned(),
            persona_prompt_file: None,
            audio_tags: false,
            max_text_length: None,
        }
    }
}

/// `[tts.xai]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct XaiTtsConfig {
    /// The voice: a built-in name or a custom cloned voice id.
    pub voice: String,
    /// The language.
    pub language: String,
    /// Speaking speed.
    #[schemars(range(min = 0.7, max = 1.5))]
    pub speed: f64,
    /// Characters per request (a positive integer; default 15,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

impl Default for XaiTtsConfig {
    fn default() -> Self {
        Self {
            voice: "eve".to_owned(),
            language: "en".to_owned(),
            speed: 1.0,
            max_text_length: None,
        }
    }
}

/// A speech provider's model and voice (unset: the provider's defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceModelConfig {
    /// The model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The voice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Characters per request (a positive integer; default by provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_text_length: Option<u64>,
}

/// The kind of a user-declared speech provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpeechProviderType {
    /// A shell command template.
    #[default]
    Command,
}

/// Audio formats a TTS command may write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TtsOutputFormat {
    /// MP3.
    #[default]
    Mp3,
    /// WAV.
    Wav,
    /// Ogg (Opus or Vorbis).
    Ogg,
    /// FLAC.
    Flac,
    /// M4A (AAC).
    M4a,
    /// Raw AAC.
    Aac,
    /// AMR.
    Amr,
    /// Opus.
    Opus,
}

impl TtsOutputFormat {
    /// The file extension.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Ogg => "ogg",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Aac => "aac",
            Self::Amr => "amr",
            Self::Opus => "opus",
        }
    }
}

/// `[tts.providers.<name>]`: a command that turns a text file into audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommandTtsConfig {
    /// Always `command`.
    #[serde(rename = "type")]
    pub kind: SpeechProviderType,
    /// The command: `{input_path}` (alias `{text_path}`), `{output_path}`,
    /// `{format}`, `{voice}` and `{model}` are replaced by values quoted
    /// for their position in the shell that runs it (the session's sh,
    /// bash or PowerShell; cmd.exe for voice messages on Windows); `{{` and
    /// `}}` are literal braces.
    pub command: String,
    /// Idle deadline: any output resets it.
    #[schemars(range(min = 1))]
    pub timeout_secs: u64,
    /// The format the command writes.
    pub output_format: TtsOutputFormat,
    /// The output can be sent as a voice bubble as is.
    pub voice_compatible: bool,
    /// Characters per run; longer text is split.
    #[schemars(range(min = 1))]
    pub max_text_length: u64,
    /// `{voice}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// `{model}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Run before first use (load a model).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_command: Option<String>,
    /// Run when the provider is released.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_command: Option<String>,
}

impl Default for CommandTtsConfig {
    fn default() -> Self {
        Self {
            kind: SpeechProviderType::Command,
            command: String::new(),
            timeout_secs: 120,
            output_format: TtsOutputFormat::Mp3,
            voice_compatible: false,
            max_text_length: 5_000,
            voice: None,
            model: None,
            warm_command: None,
            release_command: None,
        }
    }
}

/// The `[stt]` section (speech-to-text for voice input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct SttConfig {
    /// The transcription provider: `local`, `whisper-cpp`, `groq`,
    /// `openai`, `mistral`, `xai`, `elevenlabs`, `deepinfra`, `nous`, a
    /// `[stt.providers.<name>]` command or a plugin's. Set: used strictly;
    /// unset: the first available, local engines first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Spoken language (empty: detect); a provider's own `language` wins.
    pub language: String,
    /// Trim long silences before a cloud upload.
    pub cloud_trim_silence: bool,
    /// Level below which audio counts as silence, in dBFS.
    #[schemars(range(min = -90, max = 0))]
    pub cloud_trim_threshold_db: i32,
    /// Silence kept of each trimmed pause.
    pub cloud_trim_keep_ms: u64,
    /// On-device Whisper.
    pub local: LocalSttConfig,
    /// whisper.cpp (`whisper-cli` on PATH or `binary`).
    pub whisper_cpp: WhisperCppConfig,
    /// Groq (`GROQ_API_KEY`; default `whisper-large-v3-turbo`).
    pub groq: SttVendorConfig,
    /// `OpenAI` (`OPENAI_API_KEY`; default `whisper-1`, 60 s timeout).
    pub openai: SttVendorConfig,
    /// Mistral (`MISTRAL_API_KEY`; default `voxtral-mini-latest`).
    pub mistral: SttVendorConfig,
    /// xAI (`XAI_API_KEY`; default `grok-voice-transcribe-2.0`).
    pub xai: SttVendorConfig,
    /// `ElevenLabs` (`ELEVENLABS_API_KEY`; default `scribe_v2`).
    pub elevenlabs: ElevenLabsSttConfig,
    /// `DeepInfra` (`DEEPINFRA_API_KEY`).
    pub deepinfra: SttVendorConfig,
    /// The subscription tool gateway.
    pub nous: SttVendorConfig,
    /// User-declared command providers (a built-in name always wins).
    pub providers: BTreeMap<String, CommandSttConfig>,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: None,
            language: "en".to_owned(),
            cloud_trim_silence: true,
            cloud_trim_threshold_db: -40,
            cloud_trim_keep_ms: 300,
            local: LocalSttConfig::default(),
            whisper_cpp: WhisperCppConfig::default(),
            groq: SttVendorConfig::default(),
            openai: SttVendorConfig::default(),
            mistral: SttVendorConfig::default(),
            xai: SttVendorConfig::default(),
            elevenlabs: ElevenLabsSttConfig::default(),
            deepinfra: SttVendorConfig::default(),
            nous: SttVendorConfig::default(),
            providers: BTreeMap::new(),
        }
    }
}

/// Whisper model sizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum WhisperModel {
    /// `tiny`.
    #[serde(rename = "tiny")]
    Tiny,
    /// `base`.
    #[default]
    #[serde(rename = "base")]
    Base,
    /// `small`.
    #[serde(rename = "small")]
    Small,
    /// `medium`.
    #[serde(rename = "medium")]
    Medium,
    /// `large-v3`.
    #[serde(rename = "large-v3")]
    LargeV3,
    /// `turbo`.
    #[serde(rename = "turbo")]
    Turbo,
}

impl WhisperModel {
    /// The model name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Base => "base",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::LargeV3 => "large-v3",
            Self::Turbo => "turbo",
        }
    }
}

/// `[stt.local]`: on-device Whisper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LocalSttConfig {
    /// The model size.
    pub model: WhisperModel,
    /// Voice-activity filtering.
    pub vad: bool,
    /// Shortest silence the voice-activity filter splits on.
    pub vad_min_silence_ms: u64,
    /// Unload the model after this idle time (`0`: never).
    pub unload_after_idle_secs: u64,
    /// Spoken language (unset: `stt.language`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Default for LocalSttConfig {
    fn default() -> Self {
        Self {
            model: WhisperModel::Base,
            vad: true,
            vad_min_silence_ms: 500,
            unload_after_idle_secs: 0,
            language: None,
        }
    }
}

/// `[stt.whisper_cpp]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WhisperCppConfig {
    /// The `whisper-cli` executable (unset: found on PATH).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// The GGML model file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Spoken language (unset: `stt.language`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// A cloud transcription provider's model and language.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SttVendorConfig {
    /// The model (unset: the provider's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Spoken language (unset: `stt.language`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Bound on one request (unset: the provider's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub timeout_secs: Option<u64>,
}

/// `[stt.elevenlabs]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ElevenLabsSttConfig {
    /// The model (default `scribe_v2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Spoken language (unset: `stt.language`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Label speakers.
    pub diarize: bool,
    /// Tag non-speech events (`(laughter)`).
    pub tag_audio_events: bool,
}

/// Transcript formats a STT command may write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    /// Plain text.
    #[default]
    Txt,
    /// JSON with a `text` field.
    Json,
    /// `SubRip` subtitles.
    Srt,
    /// `WebVTT` subtitles.
    Vtt,
}

impl TranscriptFormat {
    /// The file extension.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Json => "json",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
        }
    }
}

/// `[stt.providers.<name>]`: a command that transcribes an audio file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommandSttConfig {
    /// Always `command`.
    #[serde(rename = "type")]
    pub kind: SpeechProviderType,
    /// The command: `{input_path}`, `{output_path}`, `{output_dir}`,
    /// `{format}`, `{language}` and `{model}` are replaced by values quoted
    /// for their position in the shell that runs it (the session's sh,
    /// bash or PowerShell; cmd.exe for voice messages on Windows); `{{` and
    /// `}}` are literal braces. The transcript is read
    /// from `{output_path}` when written, else stdout.
    pub command: String,
    /// Bound on one run.
    #[schemars(range(min = 1))]
    pub timeout_secs: u64,
    /// The transcript format.
    pub format: TranscriptFormat,
    /// Transcode the audio to 16 kHz mono first.
    pub normalize: bool,
    /// `{language}` (unset: `stt.language`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// `{model}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Default for CommandSttConfig {
    fn default() -> Self {
        Self {
            kind: SpeechProviderType::Command,
            command: String::new(),
            timeout_secs: 300,
            format: TranscriptFormat::Txt,
            normalize: false,
            language: None,
            model: None,
        }
    }
}
