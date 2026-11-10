//! Speech-to-text (spec: web-and-media-tools, Built-in speech-to-text
//! providers and resolution; Command-template speech providers).
//!
//! A set `stt.provider` is used strictly: when it cannot run, or fails,
//! the caller gets `stt is configured to use <provider>, but <reason>` and
//! no other provider is tried; a name that is neither built in nor a
//! declared command may be a provider an enabled plugin registered
//! ([`crate::plugin`]). Unset, the first available of the [`AUTO_ORDER`]
//! (on-device first) is used and named in the transcript. Cloud uploads
//! are silence-trimmed first (raw audio when trimming fails) and fitted to
//! the provider's declared upload limits (bytes and duration): re-encoded,
//! else split at pauses ([`super::fit`]).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_config::schema::{CommandSttConfig, SttConfig, TranscriptFormat};
use pmt_providers::auth::gateway::Category;
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use super::command::{self, render};
use super::{SpeechState, tempdir};
use crate::access::Access;
use crate::http::{Multipart, b64, ok_json};
use crate::plugin::SttRequest;
use crate::{MediaSession, MediaSetup};

/// The providers tried, in order, when `stt.provider` is unset.
pub const AUTO_ORDER: &[&str] = &[
    "local",
    "whisper-cpp",
    "groq",
    "openai",
    "mistral",
    "xai",
    "elevenlabs",
    "deepinfra",
];

/// Every built-in provider (a command provider of the same name is
/// ignored).
pub const BUILTIN: &[&str] = &[
    "local",
    "whisper-cpp",
    "groq",
    "openai",
    "mistral",
    "xai",
    "elevenlabs",
    "deepinfra",
    "nous",
];

/// The reason the caller gets when nothing can transcribe.
pub const NONE_AVAILABLE: &str = "no STT provider available";

const MIB: u64 = 1 << 20;

/// A transcript and who made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// The text.
    pub text: String,
    /// The provider.
    pub provider: String,
    /// Its model.
    pub model: String,
    /// How it billed.
    pub billing_source: String,
}

/// A provider ready to run.
#[derive(Debug, Clone)]
struct Listener {
    provider: String,
    model: String,
    language: Option<String>,
    /// Set only by a `pre_transcription` callback.
    prompt: Option<String>,
    access: Access,
    base_url: String,
    timeout: Duration,
}

/// The provider's API key secret.
fn secret(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "groq" => "GROQ_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "xai" => "XAI_API_KEY",
        "elevenlabs" => "ELEVENLABS_API_KEY",
        "deepinfra" => "DEEPINFRA_API_KEY",
        _ => return None,
    })
}

/// A cloud provider's per-request upload limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimit {
    /// Most bytes.
    pub bytes: u64,
    /// Longest recording, in seconds (`None`: only the size counts).
    pub seconds: Option<u64>,
}

/// The upload limits a cloud provider declares.
#[must_use]
pub fn upload_limit(provider: &str) -> Option<UploadLimit> {
    let (bytes, seconds) = match provider {
        "openai" | "groq" => (25 * MIB, None),
        "mistral" => (500 * MIB, Some(3_600)),
        "xai" => (500 * MIB, None),
        "elevenlabs" => (5 * 1024 * MIB - 1, None),
        _ => return None,
    };
    Some(UploadLimit { bytes, seconds })
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "groq" => "whisper-large-v3-turbo",
        "openai" => "whisper-1",
        "mistral" => "voxtral-mini-latest",
        "xai" => "grok-voice-transcribe-2.0",
        "elevenlabs" => "scribe_v2",
        "deepinfra" => "openai/whisper-large-v3-turbo",
        _ => "",
    }
}

fn default_base(provider: &str) -> &'static str {
    match provider {
        "groq" => "https://api.groq.com/openai/v1",
        "openai" => "https://api.openai.com/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "xai" => "https://api.x.ai/v1",
        "elevenlabs" => "https://api.elevenlabs.io/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        _ => "",
    }
}

/// `(model, language, timeout)` settings of a built-in provider.
fn settings(stt: &SttConfig, provider: &str) -> (Option<String>, Option<String>, Option<u64>) {
    let vendor = |c: &pmt_config::schema::SttVendorConfig| {
        (c.model.clone(), c.language.clone(), c.timeout_secs)
    };
    match provider {
        "local" => (
            Some(stt.local.model.as_str().to_owned()),
            stt.local.language.clone(),
            None,
        ),
        "whisper-cpp" => (
            stt.whisper_cpp.model.clone(),
            stt.whisper_cpp.language.clone(),
            None,
        ),
        "elevenlabs" => (
            stt.elevenlabs.model.clone(),
            stt.elevenlabs.language.clone(),
            None,
        ),
        "groq" => vendor(&stt.groq),
        "openai" => vendor(&stt.openai),
        "mistral" => vendor(&stt.mistral),
        "xai" => vendor(&stt.xai),
        "deepinfra" => vendor(&stt.deepinfra),
        _ => vendor(&stt.nous),
    }
}

/// The whisper.cpp executable.
fn whisper_cli(stt: &SttConfig) -> Option<PathBuf> {
    if let Some(binary) = &stt.whisper_cpp.binary {
        return Some(PathBuf::from(binary)).filter(|p| p.exists());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    pmt_tools::terminal::shell::find_executable("whisper-cli", &path, pathext.as_deref())
}

/// The built-in `provider` ready to run, or why it cannot.
fn listener(setup: &MediaSetup, provider: &str) -> Result<Listener, String> {
    let stt = &setup.config().stt;
    let (model, language, timeout) = settings(stt, provider);
    let access = match provider {
        "local" => {
            let ready = setup.local.as_ref().is_some_and(|l| l.available("whisper"));
            if !ready {
                return Err("the local Whisper runtime is not installed".to_owned());
            }
            Access::local()
        }
        "whisper-cpp" => {
            if whisper_cli(stt).is_none() {
                return Err("whisper-cli is not installed (set stt.whisper_cpp.binary)".to_owned());
            }
            if model.is_none() {
                return Err("stt.whisper_cpp.model is not set".to_owned());
            }
            Access::local()
        }
        "nous" => setup.credentials.gateway(Category::Stt)?,
        other => {
            let name = secret(other)
                .ok_or_else(|| format!("stt provider '{other}' is not installed or not enabled"))?;
            setup.credentials.key(other, name)?
        }
    };
    let language = language
        .or_else(|| Some(stt.language.clone()))
        .filter(|l| !l.is_empty());
    let default_timeout = if provider == "openai" { 60 } else { 300 };
    Ok(Listener {
        provider: provider.to_owned(),
        model: model.unwrap_or_else(|| default_model(provider).to_owned()),
        language,
        prompt: None,
        base_url: match provider {
            "nous" => access.gateway_url.clone().unwrap_or_default(),
            other => setup.endpoint(other, default_base(other)),
        },
        access,
        timeout: Duration::from_secs(timeout.unwrap_or(default_timeout)),
    })
}

impl MediaSession {
    /// The provider a transcription would use now, without running it (an
    /// empty transcript naming it), or why there is none.
    ///
    /// # Errors
    ///
    /// As [`MediaSession::transcribe`] before any audio is sent.
    pub fn stt_choice(&self) -> Result<Transcript, String> {
        let setup = self.setup();
        let stt = &setup.config().stt;
        let named = |listener: Listener| Transcript {
            text: String::new(),
            provider: listener.provider,
            model: listener.model,
            billing_source: listener.access.billing_source,
        };
        let Some(provider) = stt.provider.clone() else {
            return AUTO_ORDER
                .iter()
                .find_map(|p| listener(setup, p).ok())
                .map(named)
                .ok_or_else(|| NONE_AVAILABLE.to_owned());
        };
        if !BUILTIN.contains(&provider.as_str()) {
            let plugin = crate::plugin::selected(setup, crate::plugin::Kind::Stt);
            return match (stt.providers.get(&provider), plugin) {
                (Some(command), _) => Ok(Transcript {
                    text: String::new(),
                    provider,
                    model: command.model.clone().unwrap_or_default(),
                    billing_source: "local".to_owned(),
                }),
                (None, Some(selected)) => Ok(Transcript {
                    text: String::new(),
                    provider,
                    model: String::new(),
                    billing_source: format!("reported by plugin '{}'", selected.plugin),
                }),
                (None, None) => Err(format!(
                    "stt is configured to use {provider}, but stt provider '{provider}' is not installed or not enabled"
                )),
            };
        }
        listener(setup, &provider)
            .map(named)
            .map_err(|why| format!("stt is configured to use {provider}, but {why}"))
    }

    /// The transcript of the audio at `path`.
    ///
    /// # Errors
    ///
    /// `stt is configured to use <provider>, but <reason>` for a set
    /// provider; [`NONE_AVAILABLE`] when none is set and none can run; the
    /// chosen provider's failure.
    pub async fn transcribe(&self, path: &Path) -> Result<Transcript, String> {
        let setup = self.setup();
        let stt = &setup.config().stt;
        let Some(provider) = stt.provider.clone() else {
            let listener = AUTO_ORDER
                .iter()
                .find_map(|p| listener(setup, p).ok())
                .ok_or_else(|| NONE_AVAILABLE.to_owned())?;
            return self.listen(&self.hooked(listener).await, path).await;
        };
        let strict = |why: String| format!("stt is configured to use {provider}, but {why}");
        if !BUILTIN.contains(&provider.as_str())
            && let Some(command) = stt.providers.get(&provider)
        {
            let request = self.fire(command_request(stt, &provider, command)).await;
            return command_stt(setup, &self.speech, command, &request, path)
                .await
                .map_err(strict);
        }
        if let Some(selected) = crate::plugin::selected(setup, crate::plugin::Kind::Stt) {
            let request = self
                .fire(SttRequest {
                    provider: selected.id.clone(),
                    language: Some(stt.language.clone()).filter(|l| !l.is_empty()),
                    ..SttRequest::default()
                })
                .await;
            return crate::plugin::transcribe(setup, &selected, path, &request)
                .await
                .map_err(strict);
        }
        let listener = listener(setup, &provider).map_err(strict)?;
        self.listen(&self.hooked(listener).await, path)
            .await
            .map_err(strict)
    }

    /// Fires `pre_transcription` with the resolved `request`.
    async fn fire(&self, request: SttRequest) -> SttRequest {
        let provider = request.provider.clone();
        let mut fired = self.setup().plugins.pre_transcription(request).await;
        // The provider was resolved before the hook; it never moves.
        fired.provider = provider;
        fired
    }

    /// `listener` with the `pre_transcription` changes applied.
    async fn hooked(&self, mut listener: Listener) -> Listener {
        let request = self
            .fire(SttRequest {
                provider: listener.provider.clone(),
                model: listener.model.clone(),
                language: listener.language.clone(),
                prompt: None,
            })
            .await;
        listener.model = request.model;
        listener.language = request.language;
        listener.prompt = request.prompt;
        listener
    }

    async fn listen(&self, listener: &Listener, path: &Path) -> Result<Transcript, String> {
        let setup = self.setup();
        let text = match listener.provider.as_str() {
            "local" => self.local_whisper(listener, path).await?,
            "whisper-cpp" => whisper_cpp(setup, listener, path).await?,
            _ => self.cloud(listener, path).await?,
        };
        Ok(Transcript {
            text,
            provider: listener.provider.clone(),
            model: listener.model.clone(),
            billing_source: listener.access.billing_source.clone(),
        })
    }

    async fn local_whisper(&self, listener: &Listener, path: &Path) -> Result<String, String> {
        let setup = self.setup();
        let launcher = setup
            .local
            .as_ref()
            .ok_or("the local Whisper runtime is not installed")?;
        self.speech
            .hold(launcher.as_ref(), "whisper", self.speech.stt_idle);
        let local = &setup.config().stt.local;
        let request = json!({"op": "transcribe", "input": path, "model": listener.model,
            "language": listener.language, "prompt": listener.prompt, "vad": local.vad,
            "vad_min_silence_ms": local.vad_min_silence_ms});
        let answer = super::local::request(launcher.as_ref(), "whisper", &request).await?;
        Ok(answer["text"].as_str().unwrap_or_default().to_owned())
    }

    /// A cloud upload: trimmed when configured, then fitted to the
    /// provider's upload limits ([`super::fit`]): one upload, or pieces
    /// transcribed in order and joined.
    async fn cloud(&self, listener: &Listener, path: &Path) -> Result<String, String> {
        let setup = self.setup();
        let stt = &setup.config().stt;
        let (upload, scratch) = if stt.cloud_trim_silence {
            self.trimmed(path, stt).await
        } else {
            (path.to_path_buf(), None)
        };
        let result = self.upload(listener, path, &upload).await;
        if let Some(dir) = scratch {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        result
    }

    async fn upload(
        &self,
        listener: &Listener,
        original: &Path,
        upload: &Path,
    ) -> Result<String, String> {
        let setup = self.setup();
        let bytes = tokio::fs::read(upload)
            .await
            .map_err(|e| format!("{}: {e}", original.display()))?;
        let Some(limit) = upload_limit(&listener.provider) else {
            let name = upload
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("audio.ogg");
            return upload_audio(setup, listener, &bytes, name).await;
        };
        let pieces = self.fit(limit, &listener.provider, upload, bytes).await?;
        let mut texts = Vec::with_capacity(pieces.len());
        for piece in &pieces {
            let text = upload_audio(setup, listener, &piece.bytes, &piece.name).await?;
            if !text.is_empty() {
                texts.push(text);
            }
        }
        Ok(texts.join(" "))
    }

    /// The audio with long pauses shortened, or the original when
    /// trimming fails; the scratch directory to remove.
    async fn trimmed(&self, path: &Path, stt: &SttConfig) -> (PathBuf, Option<PathBuf>) {
        if !self.speech.ffmpeg.available().await {
            return (path.to_path_buf(), None);
        }
        let Ok(dir) = tempdir(self.setup()) else {
            return (path.to_path_buf(), None);
        };
        let extension = path.extension().and_then(OsStr::to_str).unwrap_or("wav");
        let out = dir.join(format!("trimmed.{extension}"));
        match self
            .speech
            .ffmpeg
            .trim_silence(
                path,
                &out,
                stt.cloud_trim_threshold_db,
                stt.cloud_trim_keep_ms,
            )
            .await
        {
            Ok(()) => (out, Some(dir)),
            Err(why) => {
                tracing::info!(%why, "silence trim failed; uploading the raw audio");
                let _ = tokio::fs::remove_dir_all(&dir).await;
                (path.to_path_buf(), None)
            }
        }
    }
}

fn mime_of(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
    {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "mp4" => "audio/mp4",
        "flac" => "audio/flac",
        "webm" => "audio/webm",
        _ => "audio/ogg",
    }
}

async fn upload_audio(
    setup: &MediaSetup,
    listener: &Listener,
    bytes: &[u8],
    name: &str,
) -> Result<String, String> {
    let token = listener.access.bearer()?.expose().to_owned();
    if listener.provider == "nous" {
        let routing = pmt_providers::auth::gateway::ToolRouting::Gateway {
            url: listener.base_url.clone(),
            token: listener.access.bearer()?.clone(),
            billing_source: listener.access.billing_source.clone(),
        };
        let body = json!({"audio": b64(bytes), "file_name": name, "model": listener.model,
            "language": listener.language, "prompt": listener.prompt});
        let reply = pmt_providers::auth::gateway::post(
            &setup.client,
            &routing,
            "/v1/audio/transcriptions",
            &body,
        )
        .await
        .map_err(|e| e.message().to_owned())?;
        return Ok(reply["text"].as_str().unwrap_or_default().trim().to_owned());
    }
    let mut form = Multipart::new();
    let mut request = if listener.provider == "elevenlabs" {
        let config = &setup.config().stt.elevenlabs;
        form = form
            .text("model_id", &listener.model)
            .text("diarize", &config.diarize.to_string())
            .text("tag_audio_events", &config.tag_audio_events.to_string());
        if let Some(language) = &listener.language {
            form = form.text("language_code", language);
        }
        setup
            .client
            .post(format!("{}/speech-to-text", listener.base_url))
            .header("xi-api-key", token)
    } else {
        form = form.text("model", &listener.model);
        if let Some(language) = &listener.language {
            form = form.text("language", language);
        }
        if let Some(prompt) = &listener.prompt {
            form = form.text("prompt", prompt);
        }
        setup
            .client
            .post(format!("{}/audio/transcriptions", listener.base_url))
            .bearer_auth(token)
    };
    let (kind, body) = form.file("file", name, mime_of(name), bytes).finish();
    request = request
        .header(CONTENT_TYPE, kind)
        .timeout(listener.timeout)
        .body(body);
    let reply = ok_json(request.send().await).await?;
    Ok(reply["text"].as_str().unwrap_or_default().trim().to_owned())
}

async fn whisper_cpp(
    setup: &MediaSetup,
    listener: &Listener,
    path: &Path,
) -> Result<String, String> {
    let exe = whisper_cli(&setup.config().stt).ok_or("whisper-cli is not installed")?;
    let dir = tempdir(setup)?;
    let base = dir.join("transcript");
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("-m")
        .arg(&listener.model)
        .arg("-f")
        .arg(path)
        .arg("-l")
        .arg(listener.language.as_deref().unwrap_or("auto"));
    if let Some(prompt) = &listener.prompt {
        command.arg("--prompt").arg(prompt);
    }
    command
        .args(["-otxt", "-nt", "-of"])
        .arg(&base)
        .kill_on_drop(true);
    let output = tokio::time::timeout(listener.timeout, command.output())
        .await
        .map_err(|_| "whisper-cli timed out".to_owned())?
        .map_err(|e| format!("whisper-cli could not start: {e}"))?;
    let text = tokio::fs::read_to_string(base.with_extension("txt"))
        .await
        .unwrap_or_default();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "whisper-cli failed: {}",
            super::audio::tail(&stderr)
        ));
    }
    Ok(text.trim().to_owned())
}

/// The text of a transcript in `format`.
#[must_use]
pub fn transcript_text(raw: &str, format: TranscriptFormat) -> String {
    match format {
        TranscriptFormat::Txt => raw.trim().to_owned(),
        TranscriptFormat::Json => serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v["text"].as_str().map(str::to_owned))
            .unwrap_or_else(|| raw.trim().to_owned()),
        TranscriptFormat::Srt | TranscriptFormat::Vtt => raw
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && *l != "WEBVTT"
                    && !l.contains("-->")
                    && !l.chars().all(|c| c.is_ascii_digit())
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A `[stt.providers.<name>]` command as resolved, before the hook.
fn command_request(stt: &SttConfig, name: &str, config: &CommandSttConfig) -> SttRequest {
    SttRequest {
        provider: name.to_owned(),
        model: config.model.clone().unwrap_or_default(),
        language: Some(
            config
                .language
                .clone()
                .unwrap_or_else(|| stt.language.clone()),
        )
        .filter(|l| !l.is_empty()),
        prompt: None,
    }
}

/// Runs a `[stt.providers.<name>]` command for `request`.
async fn command_stt(
    setup: &MediaSetup,
    state: &SpeechState,
    config: &CommandSttConfig,
    request: &SttRequest,
    path: &Path,
) -> Result<Transcript, String> {
    let dir = tempdir(setup)?;
    let result = run_command_stt(setup, state, (config, request), path, &dir).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(Transcript {
        text: result?,
        provider: request.provider.clone(),
        model: request.model.clone(),
        billing_source: "local".to_owned(),
    })
}

async fn run_command_stt(
    setup: &MediaSetup,
    state: &SpeechState,
    (config, request): (&CommandSttConfig, &SttRequest),
    path: &Path,
    dir: &Path,
) -> Result<String, String> {
    let input = if config.normalize {
        let out = dir.join("normalized.wav");
        state.ffmpeg.normalize(path, &out).await?;
        out
    } else {
        path.to_path_buf()
    };
    let format = config.format.extension();
    let output = dir.join(format!("transcript.{format}"));
    let values = BTreeMap::from([
        ("input_path", input.display().to_string()),
        ("output_path", output.display().to_string()),
        ("output_dir", dir.display().to_string()),
        ("format", format.to_owned()),
        ("language", request.language.clone().unwrap_or_default()),
        ("model", request.model.clone()),
        ("prompt", request.prompt.clone().unwrap_or_default()),
    ]);
    let line = render(&config.command, &values, setup.runner.syntax())?;
    let idle = Duration::from_secs(config.timeout_secs);
    let ran = command::run(setup.runner.as_ref(), &line, &setup.workspace, idle, false).await?;
    let written = tokio::fs::read_to_string(&output).await.unwrap_or_default();
    let raw = if written.trim().is_empty() {
        ran.stdout
    } else {
        written
    };
    if raw.trim().is_empty() {
        return Err("command STT provider wrote no output file and produced no stdout".to_owned());
    }
    Ok(transcript_text(&raw, config.format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_and_json_transcripts_become_text() {
        let srt =
            "1\n00:00:00,000 --> 00:00:01,000\nhello\n\n2\n00:00:01,000 --> 00:00:02,000\nworld\n";
        assert_eq!(transcript_text(srt, TranscriptFormat::Srt), "hello world");
        let vtt = "WEBVTT\n\n00:00.000 --> 00:01.000\nhi there\n";
        assert_eq!(transcript_text(vtt, TranscriptFormat::Vtt), "hi there");
        assert_eq!(
            transcript_text(r#"{"text": "json"}"#, TranscriptFormat::Json),
            "json"
        );
        assert_eq!(upload_limit("local"), None);
    }

    #[test]
    fn cloud_providers_declare_byte_and_duration_limits() {
        let limit = |p| upload_limit(p).map(|l| (l.bytes, l.seconds));
        assert_eq!(limit("openai"), Some((25 << 20, None)));
        assert_eq!(limit("groq"), Some((25 << 20, None)));
        assert_eq!(limit("mistral"), Some((500 << 20, Some(3_600))));
        assert_eq!(limit("xai"), Some((500 << 20, None)));
        assert_eq!(limit("elevenlabs"), Some(((5 << 30) - 1, None)));
        assert_eq!(limit("local"), None);
        assert_eq!(limit("whisper-cpp"), None);
    }
}
