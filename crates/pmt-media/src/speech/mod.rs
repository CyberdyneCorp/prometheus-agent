//! Speech (spec: web-and-media-tools, `text_to_speech` tool and delivery;
//! Built-in text-to-speech providers; Built-in speech-to-text providers and
//! resolution; Command-template speech providers).
//!
//! [`MediaSession::synthesize`] speaks a text of any length (chunked under
//! the provider's cap, joined into one file) and
//! [`MediaSession::transcribe`] turns audio into text; the
//! `text_to_speech` tool ([`tool`]) and the gateway's voice messages both
//! use them. [`SpeechState`] is a session's hold on local engines and
//! warmed command providers.

pub mod audio;
pub mod chunk;
pub mod command;
pub mod command_tts;
pub mod edge;
pub mod local;
pub mod stt;
pub mod tool;
pub mod tts;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::Config;
use serde_json::json;

use crate::{MediaSession, MediaSetup};
use audio::{Ffmpeg, Format, is_opus};
use tts::{Speak, Voice};

/// Surfaces whose adapters render voice bubbles (Opus in Ogg).
pub const VOICE_BUBBLES: &[&str] = &["telegram", "discord"];

/// Surfaces that show the saved file's path (others get a deliverable
/// attachment in the blob store).
pub const INTERACTIVE: &[&str] = &["cli", "tui"];

async fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        pmt_core::private_fs::create_dir(parent).map_err(|e| e.to_string())?;
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A command provider's `release_command`: the line, its directory and
/// the runner it goes through.
type Release = (String, PathBuf, Arc<dyn command::CommandRunner>);

/// A session's hold on speech resources.
pub struct SpeechState {
    /// Converts to voice notes and trims silence.
    pub ffmpeg: Ffmpeg,
    keep_warm: Option<Duration>,
    stt_idle: Option<Duration>,
    leases: Mutex<Vec<local::Lease>>,
    held: Mutex<BTreeSet<String>>,
    warmed: Mutex<BTreeSet<String>>,
    release: Mutex<Vec<Release>>,
}

impl SpeechState {
    /// The state of a session of `config`, with ffmpeg from the `PATH`.
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let idle = config.stt.local.unload_after_idle_secs;
        Self {
            ffmpeg: Ffmpeg::on_path(),
            keep_warm: Some(Duration::from_secs(config.tts.keep_warm_secs)),
            stt_idle: (idle > 0).then(|| Duration::from_secs(idle)),
            leases: Mutex::default(),
            held: Mutex::default(),
            warmed: Mutex::default(),
            release: Mutex::default(),
        }
    }

    /// Takes the session's lease on `engine` once.
    fn hold(&self, engine: &str, keep_warm: Option<Duration>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.insert(engine.to_owned()) {
            self.leases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(local::Lease::acquire(engine, keep_warm));
        }
    }

    /// Whether the command provider `name` still needs its warm-up; marks
    /// it warmed and remembers its release command.
    fn first_use(&self, name: &str, release: Option<Release>) -> bool {
        let fresh = self
            .warmed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned());
        if fresh && let Some(release) = release {
            self.release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(release);
        }
        fresh
    }

    async fn speak_locally(
        &self,
        setup: &MediaSetup,
        voice: &Voice,
        text: &str,
        options: &Speak,
    ) -> Result<Vec<u8>, String> {
        let launcher = setup.local.as_ref().ok_or_else(|| {
            format!(
                "{} is not installed (no local speech runtime)",
                voice.provider
            )
        })?;
        self.hold(&voice.provider, self.keep_warm);
        let dir = tempdir(setup)?;
        let out = dir.join("out.wav");
        let request = json!({"op": "speak", "text": text, "voice": voice.voice, "model": voice.model,
            "speed": options.speed, "out": out});
        local::request(launcher.as_ref(), &voice.provider, &request).await?;
        let bytes = tokio::fs::read(&out)
            .await
            .map_err(|e| format!("{}: {e}", voice.provider))?;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(bytes)
    }
}

impl Drop for SpeechState {
    fn drop(&mut self) {
        let releases =
            std::mem::take(&mut *self.release.lock().unwrap_or_else(PoisonError::into_inner));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for (line, cwd, runner) in releases {
            runtime.spawn(async move {
                if let Err(why) =
                    command::run(runner.as_ref(), &line, &cwd, Duration::from_mins(1), false).await
                {
                    tracing::warn!(%why, "tts release_command failed");
                }
            });
        }
    }
}

/// A private scratch directory under the profile's audio cache.
fn tempdir(setup: &MediaSetup) -> Result<PathBuf, String> {
    let dir = audio_dir(&setup.profile_dir).join(format!("tmp-{}", ulid::Ulid::generate()));
    pmt_core::private_fs::create_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// `<profile>/cache/audio`.
#[must_use]
pub fn audio_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("audio")
}

/// One synthesized file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    /// Where it is.
    pub path: PathBuf,
    /// Its MIME type.
    pub mime: String,
    /// It is Opus in Ogg (a voice note as is).
    pub voice_compatible: bool,
    /// The provider.
    pub provider: String,
    /// Its model.
    pub model: String,
    /// How it billed.
    pub billing_source: String,
    /// Requests sent (one per chunk).
    pub chunks: usize,
}

impl MediaSession {
    /// Speaks `text` with the session's provider into `output`, else a new
    /// file: under `<profile>/cache/audio/` on interactive surfaces, in the
    /// blob store (a deliverable attachment) on messaging platforms. On a
    /// voice-bubble platform the audio is converted to Opus when the
    /// provider made another format and ffmpeg can.
    ///
    /// # Errors
    ///
    /// The provider is unknown, unconfigured or failed, or the file could
    /// not be written.
    pub async fn synthesize(
        &self,
        text: &str,
        options: &Speak,
        output: Option<PathBuf>,
    ) -> Result<Spoken, String> {
        let setup = self.setup();
        let voice = tts::resolve(setup)?;
        let parts = chunk::chunks(text, voice.cap);
        if parts.is_empty() {
            return Err("nothing to say: the text is empty".to_owned());
        }
        let (audio, billing_source) = self.speak_parts(&voice, &parts, options).await?;
        let bytes = audio::join(audio)?;
        let to_blob = output.is_none() && !INTERACTIVE.contains(&setup.surface.as_str());
        let path = output.unwrap_or_else(|| {
            audio_dir(&setup.profile_dir).join(format!(
                "speech-{}.{}",
                ulid::Ulid::generate(),
                tts::extension(&bytes, &voice)
            ))
        });
        write(&path, &bytes).await?;
        let mut spoken = Spoken {
            mime: Format::sniff(&bytes).mime().to_owned(),
            voice_compatible: is_opus(&bytes)
                || voice.command.as_ref().is_some_and(|c| c.voice_compatible),
            path,
            provider: voice.provider.clone(),
            model: voice.model.clone(),
            billing_source,
            chunks: parts.len(),
        };
        if !spoken.voice_compatible
            && VOICE_BUBBLES.contains(&setup.surface.as_str())
            && let Some(note) = audio::voice_note(&self.speech.ffmpeg, &spoken.path).await
        {
            if note != spoken.path {
                let _ = tokio::fs::remove_file(&spoken.path).await;
            }
            spoken.path = note;
            "audio/ogg".clone_into(&mut spoken.mime);
            spoken.voice_compatible = true;
        }
        if to_blob {
            let final_bytes = tokio::fs::read(&spoken.path)
                .await
                .map_err(|e| e.to_string())?;
            let stored = setup.host.store_media(&final_bytes, &spoken.mime)?;
            let _ = tokio::fs::remove_file(&spoken.path).await;
            spoken.path = stored;
        }
        Ok(spoken)
    }

    /// The audio of each part, and how the calls billed (a plugin
    /// provider reports it with its answers).
    async fn speak_parts(
        &self,
        voice: &Voice,
        parts: &[String],
        options: &Speak,
    ) -> Result<(Vec<Vec<u8>>, String), String> {
        let setup = self.setup();
        let mut billing = voice.access.billing_source.clone();
        let mut audio = Vec::with_capacity(parts.len());
        for part in parts {
            let bytes = match &voice.plugin {
                Some(plugin) => {
                    let (bytes, billed) =
                        crate::plugin::speak(setup, voice, plugin, part, options).await?;
                    billing = billed;
                    bytes
                }
                None => tts::speak(setup, &self.speech, voice, part, options).await?,
            };
            audio.push(bytes);
        }
        Ok((audio, billing))
    }

    /// The session's speech state.
    #[must_use]
    pub fn speech(&self) -> &SpeechState {
        &self.speech
    }
}
