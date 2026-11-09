//! The text-to-speech providers (spec: web-and-media-tools, Built-in
//! text-to-speech providers; Command-template speech providers).
//!
//! [`resolve`] picks the provider of `tts.provider` (built-in names win
//! over a same-named `[tts.providers.<name>]` command; unset detects one,
//! else the keyless `edge`), its model, voice, per-request cap and
//! credential; [`speak`] asks it for the audio of one chunk.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use pmt_config::schema::{CommandTtsConfig, TtsConfig, VoiceModelConfig};
use pmt_providers::auth::gateway::Category;
use reqwest::header::{CONTENT_TYPE, HeaderName};
use serde_json::{Value, json};

use super::audio::{Format, wav_from_pcm};
use crate::MediaSetup;
use crate::access::Access;
use crate::http::{ok_bytes, unb64};

/// The built-in provider ids (a command provider of the same name is
/// ignored).
pub const BUILTIN: &[&str] = &[
    "edge",
    "elevenlabs",
    "openai",
    "gemini",
    "xai",
    "mistral",
    "minimax",
    "deepinfra",
    "nous",
    "piper",
    "kittentts",
    "neutts",
];

/// The engines that run on this machine.
pub const LOCAL: &[&str] = &["piper", "kittentts", "neutts"];

/// The provider used when nothing is configured or detected.
pub const DEFAULT: &str = "edge";

/// One resolved speech provider.
#[derive(Debug, Clone)]
pub struct Voice {
    /// The provider id.
    pub provider: String,
    /// The model (empty when the provider has none).
    pub model: String,
    /// The voice.
    pub voice: String,
    /// Characters per request.
    pub cap: usize,
    /// The credential and billing.
    pub access: Access,
    /// The API base URL (HTTP providers).
    pub base_url: String,
    /// The command of a command provider.
    pub command: Option<CommandTtsConfig>,
    /// The plugin that registered a plugin provider.
    pub plugin: Option<String>,
}

/// Per-call options.
#[derive(Debug, Clone, Default)]
pub struct Speak {
    /// Speaking speed (0.25..4.0).
    pub speed: Option<f64>,
    /// Style direction.
    pub instructions: Option<String>,
    /// A voice other than the configured one (`elevenlabs`).
    pub voice: Option<String>,
}

/// The default cap of `provider` running `model`.
#[must_use]
pub fn default_cap(provider: &str, model: &str) -> usize {
    match (provider, model) {
        ("elevenlabs", "eleven_flash_v2_5") => 40_000,
        ("elevenlabs", "eleven_flash_v2") => 30_000,
        ("elevenlabs", "eleven_v3") => 5_000,
        ("elevenlabs" | "minimax", _) => 10_000,
        ("openai" | "deepinfra", _) => 4_096,
        ("gemini", _) => 32_000,
        ("xai", _) => 15_000,
        ("mistral", _) => 4_000,
        ("kittentts" | "neutts", _) => 2_000,
        _ => 5_000,
    }
}

fn cap(configured: Option<u64>, provider: &str, model: &str) -> usize {
    configured
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or_else(|| default_cap(provider, model))
}

/// `(model, voice, cap override)` of a provider's settings, with the
/// spec's defaults.
fn settings(tts: &TtsConfig, provider: &str) -> (String, String, Option<u64>) {
    let pick = |c: &VoiceModelConfig, model: &str, voice: &str| {
        (
            c.model.clone().unwrap_or_else(|| model.to_owned()),
            c.voice.clone().unwrap_or_else(|| voice.to_owned()),
            c.max_text_length,
        )
    };
    match provider {
        "edge" => (
            String::new(),
            tts.edge.voice.clone(),
            tts.edge.max_text_length,
        ),
        "elevenlabs" => (
            tts.elevenlabs.model_id.clone(),
            tts.elevenlabs.voice_id.clone(),
            tts.elevenlabs.max_text_length,
        ),
        "openai" => (
            tts.openai.model.clone(),
            tts.openai.voice.clone(),
            tts.openai.max_text_length,
        ),
        "gemini" => (
            tts.gemini.model.clone(),
            tts.gemini.voice.clone(),
            tts.gemini.max_text_length,
        ),
        "xai" => (
            "grok-tts".to_owned(),
            tts.xai.voice.clone(),
            tts.xai.max_text_length,
        ),
        "mistral" => pick(&tts.mistral, "voxtral-mini-tts-2603", ""),
        "minimax" => pick(&tts.minimax, "speech-02-hd", "male-qn-qingse"),
        "deepinfra" => pick(&tts.deepinfra, "hexgrad/Kokoro-82M", "af_heart"),
        "nous" => pick(&tts.nous, "", ""),
        "piper" => pick(&tts.piper, "", "en_US-lessac-medium"),
        "kittentts" => pick(
            &tts.kittentts,
            "KittenML/kitten-tts-nano-0.2",
            "expr-voice-2-f",
        ),
        _ => pick(&tts.neutts, "neuphonic/neutts-air", ""),
    }
}

fn default_base(provider: &str) -> &'static str {
    match provider {
        "edge" => super::edge::EDGE_URL,
        "elevenlabs" => "https://api.elevenlabs.io/v1",
        "openai" => "https://api.openai.com/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta",
        "xai" => "https://api.x.ai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "minimax" => "https://api.minimax.io/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        _ => "",
    }
}

/// The API key secret of a cloud speech provider.
#[must_use]
pub fn secret(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "elevenlabs" => "ELEVENLABS_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "xai" => "XAI_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "deepinfra" => "DEEPINFRA_API_KEY",
        _ => return None,
    })
}

/// The session's speech provider: a built-in one, else a declared command
/// provider, else one an enabled plugin registered.
///
/// # Errors
///
/// An unknown provider (fail closed) or a missing credential.
pub fn resolve(setup: &MediaSetup) -> Result<Voice, String> {
    let tts = &setup.config().tts;
    let choice = setup
        .credentials
        .choose(Category::Tts, tts.provider.as_deref(), DEFAULT);
    let provider = choice.provider;
    if !BUILTIN.contains(&provider.as_str()) {
        let Some(command) = tts.providers.get(&provider) else {
            return plugin_voice(setup, provider);
        };
        return Ok(Voice {
            model: command.model.clone().unwrap_or_default(),
            voice: command.voice.clone().unwrap_or_default(),
            cap: usize::try_from(command.max_text_length).unwrap_or(usize::MAX),
            access: Access::local(),
            base_url: String::new(),
            command: Some(command.clone()),
            plugin: None,
            provider,
        });
    }
    let (model, voice, configured) = settings(tts, &provider);
    let access = match provider.as_str() {
        "edge" => Access::keyless("edge"),
        "nous" => setup.credentials.gateway(Category::Tts)?,
        local if LOCAL.contains(&local) => Access::local(),
        other => setup
            .credentials
            .key(other, secret(other).unwrap_or_default())?,
    };
    let base_url = match (provider.as_str(), &tts.openai.base_url) {
        ("nous", _) => access.gateway_url.clone().unwrap_or_default(),
        ("openai", Some(base)) => base.trim_end_matches('/').to_owned(),
        (other, _) => setup.endpoint(other, default_base(other)),
    };
    Ok(Voice {
        cap: cap(configured, &provider, &model),
        provider,
        model,
        voice,
        access,
        base_url,
        command: None,
        plugin: None,
    })
}

/// The provider `provider` an enabled plugin registered; how it bills is
/// known from its answers.
fn plugin_voice(setup: &MediaSetup, provider: String) -> Result<Voice, String> {
    let kind = crate::plugin::Kind::Tts;
    let plugin = setup
        .plugins
        .plugin_of(kind, &provider)
        .ok_or_else(|| crate::plugin::not_installed(kind, &provider))?;
    Ok(Voice {
        model: String::new(),
        voice: String::new(),
        cap: default_cap(&provider, ""),
        access: Access::local(),
        base_url: String::new(),
        command: None,
        plugin: Some(plugin),
        provider,
    })
}

/// The audio of one chunk.
///
/// # Errors
///
/// The provider's failure.
pub async fn speak(
    setup: &MediaSetup,
    state: &super::SpeechState,
    voice: &Voice,
    text: &str,
    options: &Speak,
) -> Result<Vec<u8>, String> {
    let tts = &setup.config().tts;
    match voice.provider.as_str() {
        _ if voice.command.is_some() => super::command_tts::speak(setup, state, voice, text).await,
        "edge" => {
            let speed = options.speed.unwrap_or(1.0);
            super::edge::synthesize(&voice.base_url, text, &voice.voice, speed).await
        }
        "elevenlabs" => elevenlabs(setup, voice, text, options).await,
        "gemini" => gemini(setup, voice, text).await,
        "minimax" => minimax(setup, voice, text).await,
        "nous" => gateway(setup, voice, text, options).await,
        local if LOCAL.contains(&local) => state.speak_locally(setup, voice, text, options).await,
        provider => {
            let speed = options.speed.or(match provider {
                "openai" => tts.openai.speed,
                "xai" => Some(tts.xai.speed),
                _ => None,
            });
            speech_api(setup, voice, text, speed, options.instructions.as_deref()).await
        }
    }
}

async fn post(
    setup: &MediaSetup,
    url: &str,
    auth: (&str, String),
    body: &Value,
) -> Result<(Vec<u8>, String), String> {
    let name = HeaderName::try_from(auth.0).map_err(|e| e.to_string())?;
    ok_bytes(
        setup
            .client
            .post(url)
            .header(name, auth.1)
            .header(CONTENT_TYPE, "application/json")
            .timeout(Duration::from_mins(3))
            .body(body.to_string())
            .send()
            .await,
    )
    .await
}

fn bearer(voice: &Voice) -> Result<(&'static str, String), String> {
    Ok((
        "authorization",
        format!("Bearer {}", voice.access.bearer()?.expose()),
    ))
}

/// Audio bytes of a reply that is either audio or JSON carrying base64.
fn audio_of(body: Vec<u8>, mime: &str) -> Result<Vec<u8>, String> {
    if !mime.contains("json") {
        return Ok(body);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| format!("unreadable reply: {e}"))?;
    let b64 = ["/audio_data", "/audio", "/data/audio"]
        .iter()
        .find_map(|p| value.pointer(p).and_then(Value::as_str))
        .ok_or("the reply holds no audio")?;
    unb64(b64)
}

/// The OpenAI speech API and its compatible endpoints (`openai`, `xai`,
/// `mistral`, `deepinfra`).
async fn speech_api(
    setup: &MediaSetup,
    voice: &Voice,
    text: &str,
    speed: Option<f64>,
    instructions: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut body = json!({"model": voice.model, "input": text, "response_format": "mp3"});
    if !voice.voice.is_empty() {
        body["voice"] = json!(voice.voice);
    }
    if let Some(speed) = speed {
        body["speed"] = json!(speed);
    }
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if voice.provider == "xai" {
        body["language"] = json!(setup.config().tts.xai.language);
    }
    let (bytes, mime) = post(
        setup,
        &format!("{}/audio/speech", voice.base_url),
        bearer(voice)?,
        &body,
    )
    .await?;
    audio_of(bytes, &mime)
}

async fn elevenlabs(
    setup: &MediaSetup,
    voice: &Voice,
    text: &str,
    options: &Speak,
) -> Result<Vec<u8>, String> {
    let chosen = options.voice.as_deref().unwrap_or(&voice.voice);
    let mut body = json!({"text": text, "model_id": voice.model});
    if let Some(speed) = options.speed {
        body["voice_settings"] = json!({"speed": speed});
    }
    let key = voice.access.bearer()?.expose().to_owned();
    let url = format!(
        "{}/text-to-speech/{chosen}?output_format=mp3_44100_128",
        voice.base_url
    );
    post(setup, &url, ("xi-api-key", key), &body)
        .await
        .map(|(b, _)| b)
}

/// The voices of the `ElevenLabs` account: `(id, name)`.
///
/// # Errors
///
/// The API's failure.
pub async fn elevenlabs_voices(
    setup: &MediaSetup,
    voice: &Voice,
) -> Result<Vec<(String, String)>, String> {
    let key = voice.access.bearer()?.expose().to_owned();
    let reply = crate::http::ok_json(
        setup
            .client
            .get(format!("{}/voices", voice.base_url))
            .header("xi-api-key", key)
            .send()
            .await,
    )
    .await?;
    Ok(reply["voices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            (
                v["voice_id"].as_str().unwrap_or_default().to_owned(),
                v["name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect())
}

async fn gemini(setup: &MediaSetup, voice: &Voice, text: &str) -> Result<Vec<u8>, String> {
    let gemini = &setup.config().tts.gemini;
    let prompt = match &gemini.persona_prompt_file {
        Some(file) => {
            let path = setup.profile_dir.join(file);
            let template = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("tts.gemini.persona_prompt_file {}: {e}", path.display()))?;
            template.replace("{transcript}", text)
        }
        None => text.to_owned(),
    };
    let prompt = if gemini.audio_tags {
        prompt
    } else {
        strip_tags(&prompt)
    };
    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"responseModalities": ["AUDIO"], "speechConfig": {
            "voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice.voice}}}},
    });
    let key = voice.access.bearer()?.expose().to_owned();
    let url = format!("{}/models/{}:generateContent", voice.base_url, voice.model);
    let (bytes, _) = post(setup, &url, ("x-goog-api-key", key), &body).await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("unreadable reply: {e}"))?;
    let data = value
        .pointer("/candidates/0/content/parts/0/inlineData/data")
        .and_then(Value::as_str)
        .ok_or("the reply holds no audio")?;
    // Gemini speaks 16-bit mono PCM at 24 kHz.
    Ok(wav_from_pcm(&unb64(data)?, 24_000))
}

/// Text without `[bracketed]` audio tags.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// `MiniMax` answers hex-encoded audio.
async fn minimax(setup: &MediaSetup, voice: &Voice, text: &str) -> Result<Vec<u8>, String> {
    let body = json!({"model": voice.model, "text": text, "stream": false,
        "voice_setting": {"voice_id": voice.voice}, "audio_setting": {"format": "mp3"}});
    let (bytes, _) = post(
        setup,
        &format!("{}/t2a_v2", voice.base_url),
        bearer(voice)?,
        &body,
    )
    .await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("unreadable reply: {e}"))?;
    let hex = value
        .pointer("/data/audio")
        .and_then(Value::as_str)
        .ok_or("the reply holds no audio")?;
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

async fn gateway(
    setup: &MediaSetup,
    voice: &Voice,
    text: &str,
    options: &Speak,
) -> Result<Vec<u8>, String> {
    let routing = pmt_providers::auth::gateway::ToolRouting::Gateway {
        url: voice.base_url.clone(),
        token: voice.access.bearer()?.clone(),
        billing_source: voice.access.billing_source.clone(),
    };
    let body = json!({"input": text, "model": voice.model, "voice": voice.voice,
        "speed": options.speed, "format": "mp3"});
    let reply =
        pmt_providers::auth::gateway::post(&setup.client, &routing, "/v1/audio/speech", &body)
            .await
            .map_err(|e| e.message().to_owned())?;
    let audio = reply["audio"].as_str().ok_or("the reply holds no audio")?;
    unb64(audio)
}

/// The output extension of `voice`'s audio (sniffed bytes win).
#[must_use]
pub fn extension(bytes: &[u8], voice: &Voice) -> String {
    match (Format::sniff(bytes), &voice.command) {
        (Format::Other, Some(command)) => command.output_format.extension().to_owned(),
        (format, _) => format.extension().to_owned(),
    }
}

/// The placeholders of a TTS command run.
#[must_use]
pub fn command_values(
    voice: &Voice,
    input: &Path,
    output: &Path,
    format: &str,
) -> BTreeMap<&'static str, String> {
    let input = input.display().to_string();
    BTreeMap::from([
        ("input_path", input.clone()),
        ("text_path", input),
        ("output_path", output.display().to_string()),
        ("format", format.to_owned()),
        ("voice", voice.voice.clone()),
        ("model", voice.model.clone()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_follow_the_provider_and_model() {
        assert_eq!(default_cap("openai", "gpt-4o-mini-tts"), 4_096);
        assert_eq!(default_cap("elevenlabs", "eleven_flash_v2_5"), 40_000);
        assert_eq!(default_cap("elevenlabs", "eleven_v3"), 5_000);
        assert_eq!(default_cap("elevenlabs", "something_new"), 10_000);
        assert_eq!(default_cap("edge", ""), 5_000);
        assert_eq!(cap(Some(9_000), "openai", "x"), 9_000);
        assert_eq!(strip_tags("hi [laughs] there"), "hi  there");
    }
}
