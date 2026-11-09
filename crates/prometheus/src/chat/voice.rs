//! Voice mode's host side (spec: voice-mode): the profile's speech
//! providers (the session's [`MediaSession`]) and this machine's audio
//! behind the ports of [`pmt_voice`]. The line interface and the TUI both
//! build their conversations from here.
//!
//! Recordings are written to the profile's `cache/voice/` (owner-only)
//! for the transcriber and deleted as soon as it has read them.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_media::MediaSession;
use pmt_media::speech::tts::{self, Speak};
use pmt_voice::converse::Kit;
use pmt_voice::device::{BoxFuture, Capture, Clip, Microphone, Player, Sound, Synth, Transcriber};
use pmt_voice::fixture::{FixtureMicrophone, FixturePlayer};
use pmt_voice::mode::{Command, Facts, VoiceMode};
use pmt_voice::pcm;
use pmt_voice::record::Meter;
use pmt_voice::system::{SystemMicrophone, SystemPlayer};

/// `<profile>/cache/voice`.
#[must_use]
pub fn voice_dir(profile: &Path) -> PathBuf {
    profile.join("cache").join("voice")
}

/// This machine's microphone and speakers.
#[derive(Clone)]
pub struct Audio {
    /// Records.
    pub mic: Arc<dyn Microphone>,
    /// Plays cues and replies.
    pub player: Arc<dyn Player>,
}

/// The audio of this machine: recorded fixtures under the debug-build
/// test seam, else the recorder and player tools found here (ffmpeg from
/// the managed runtime or the `PATH`).
pub async fn audio(scope: &ProfileScope, test_audio: Option<&Path>) -> Audio {
    if let Some(dir) = test_audio {
        return Audio {
            mic: Arc::new(FixtureMicrophone::new(dir)),
            player: Arc::new(FixturePlayer::new(dir)),
        };
    }
    let ffmpeg = crate::chat::session::media::ffmpeg(scope)
        .exe()
        .await
        .cloned();
    let mic: Arc<dyn Microphone> = match SystemMicrophone::detect(ffmpeg.clone()) {
        Some(mic) => Arc::new(mic),
        None => Arc::new(NoMicrophone),
    };
    let player: Arc<dyn Player> = match SystemPlayer::detect(ffmpeg, &voice_dir(scope.dir())) {
        Some(player) => Arc::new(player),
        None => Arc::new(NoPlayer),
    };
    Audio { mic, player }
}

/// A host without recording tools: no input device.
struct NoMicrophone;

impl Microphone for NoMicrophone {
    fn devices(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }

    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Capture>, String>> {
        Box::pin(async { Err(pmt_voice::mode::NO_DEVICE.to_owned()) })
    }
}

/// A host without a player: replies stay text.
struct NoPlayer;

impl Player for NoPlayer {
    fn play(&self, _clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Err("no audio player found (ffplay, afplay, paplay, aplay)".to_owned()) })
    }
}

/// What `/voice on` and `/voice status` check: the input device and the
/// providers the session would use.
pub async fn facts(media: Option<&MediaSession>, mic: &dyn Microphone) -> Facts {
    let none = || Err("the session has no media tools".to_owned());
    Facts {
        device: mic.devices().await.into_iter().next(),
        stt: media.map_or_else(none, |m| m.stt_choice().map(|t| t.provider)),
        tts: media.map_or_else(none, |m| tts::resolve(m.setup()).map(|v| v.provider)),
    }
}

/// Speech-to-text through the session's provider.
pub struct Ears {
    media: Arc<MediaSession>,
    dir: PathBuf,
}

impl Ears {
    /// Transcribes with `media`, writing recordings under `profile`.
    #[must_use]
    pub fn new(media: Arc<MediaSession>, profile: &Path) -> Self {
        Self {
            media,
            dir: voice_dir(profile),
        }
    }
}

/// Writes `bytes` to a new owner-only file at `path`.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        pmt_core::private_fs::create_dir(parent)?;
    }
    let mut file = pmt_core::private_fs::new_file_options().open(path)?;
    file.write_all(bytes)
}

impl Transcriber for Ears {
    fn transcribe<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let path = self
                .dir
                .join(format!("recording-{}.wav", ulid::Ulid::generate()));
            write_private(&path, &pcm::wav(samples, pcm::RATE)).map_err(|e| e.to_string())?;
            let heard = self.media.transcribe(&path).await;
            let _ = std::fs::remove_file(&path);
            heard.map(|transcript| transcript.text)
        })
    }
}

/// Text-to-speech through the session's provider: streamed PCM when it
/// streams, else one file per sentence.
pub struct Voice {
    media: Arc<MediaSession>,
}

impl Voice {
    /// Speaks with `media`.
    #[must_use]
    pub fn new(media: Arc<MediaSession>) -> Self {
        Self { media }
    }
}

impl Synth for Voice {
    fn synthesize<'a>(&'a self, sentence: &'a str) -> BoxFuture<'a, Result<Sound, String>> {
        Box::pin(async move {
            let options = Speak::default();
            if let Some(streamed) = self.media.speak_streaming(sentence, &options).await {
                return streamed.map(|(rate, chunks)| Sound::Stream { rate, chunks });
            }
            let spoken = self.media.synthesize(sentence, &options, None).await?;
            Ok(Sound::Scratch(spoken.path))
        })
    }
}

/// The profile's speech providers for voice mode on `surface` (`cli`,
/// `tui`), outside any session: the same `[stt]` and `[tts]` resolution
/// sessions use, through the profile's network settings, with command
/// providers sandboxed like the gateway's.
#[must_use]
pub fn media(scope: &ProfileScope, surface: &str, cwd: &Path) -> Option<Arc<MediaSession>> {
    use crate::chat::session::media;
    let mut setup = pmt_media::explain::inspection_setup(
        media::credentials(scope),
        scope.dir(),
        Some(media::launcher(scope)),
    )?;
    let network = &scope.config().network;
    setup.client = pmt_net::client(&pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    })
    .ok()?;
    surface.clone_into(&mut setup.surface);
    setup.workspace = cwd.to_path_buf();
    setup.runner = Arc::new(media::ProfileRunner(scope.clone()));
    setup.plugins = crate::chat::session::media_plugins::PluginMedia::shared(
        crate::plugins_host::for_session(scope, cwd)
            .current()
            .providers
            .clone(),
        pmt_plugins::host::CallInfo {
            session_id: None,
            surface: surface.to_owned(),
            cwd: Some(cwd.display().to_string()),
        },
    );
    Some(Arc::new(
        MediaSession::new(setup).with_ffmpeg(media::ffmpeg(scope)),
    ))
}

/// Voice mode on one interactive client: its `/voice` state and, once
/// first used, the audio and speech providers its conversations use.
pub struct VoiceClient {
    /// On or off, spoken replies or not.
    pub mode: VoiceMode,
    scope: ProfileScope,
    test_audio: Option<PathBuf>,
    cwd: PathBuf,
    surface: &'static str,
    parts: Option<(Audio, Option<Arc<MediaSession>>)>,
}

impl VoiceClient {
    /// Voice mode, off, for `surface` of `scope` working in `cwd`.
    #[must_use]
    pub fn new(
        scope: &ProfileScope,
        launch: &LaunchEnv,
        cwd: &Path,
        surface: &'static str,
    ) -> Self {
        Self {
            mode: VoiceMode::default(),
            scope: scope.clone(),
            test_audio: launch.test_audio_dir().cloned(),
            cwd: cwd.to_path_buf(),
            surface,
            parts: None,
        }
    }

    async fn parts(&mut self) -> (Audio, Option<Arc<MediaSession>>) {
        if self.parts.is_none() {
            let audio = audio(&self.scope, self.test_audio.as_deref()).await;
            let media = media(&self.scope, self.surface, &self.cwd);
            self.parts = Some((audio, media));
        }
        self.parts
            .clone()
            .unwrap_or_else(|| unreachable!("set above"))
    }

    /// Runs `/voice <args>`; the line to show.
    pub async fn command(&mut self, args: &str) -> String {
        let command = match Command::parse(args) {
            Ok(command) => command,
            Err(usage) => return usage,
        };
        let facts = if command.needs_facts(&self.mode) {
            let (audio, media) = self.parts().await;
            Some(facts(media.as_deref(), audio.mic.as_ref()).await)
        } else {
            None
        };
        let config = self.scope.config().voice.clone();
        self.mode.run(command, facts.as_ref(), &config)
    }

    /// What a conversation runs with, `meter` showing the input level.
    ///
    /// # Errors
    ///
    /// The profile cannot build its speech providers.
    pub async fn kit(&mut self, meter: Meter) -> Result<Kit, String> {
        let (audio, media) = self.parts().await;
        let media = media.ok_or("the profile cannot reach its speech providers")?;
        Ok(Kit {
            mic: audio.mic,
            ears: Arc::new(Ears::new(Arc::clone(&media), self.scope.dir())),
            player: audio.player,
            voice: self
                .mode
                .spoken
                .then(|| Arc::new(Voice::new(media)) as Arc<dyn Synth>),
            meter,
            config: self.scope.config().voice.clone(),
        })
    }
}

/// The meter line while recording: `● REC 3.4s ▅▅▅▅▅▁▁▁ speech`.
#[must_use]
pub fn meter_line(level: pmt_voice::record::Level, key: &str) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bars = ((level.rms.max(1.0).log10() / 4.0) * 8.0).clamp(0.0, 8.0) as usize;
    #[allow(clippy::cast_precision_loss)]
    let seconds = level.elapsed_ms as f64 / 1_000.0;
    format!(
        "● REC {seconds:.1}s {}{} {} · {key} stops",
        "▅".repeat(bars),
        "▁".repeat(8 - bars),
        if level.speech { "speech" } else { "listening" }
    )
}
