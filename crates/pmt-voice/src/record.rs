//! One push-to-talk recording (spec: voice-mode, Push-to-talk recording
//! and continuous loop; Two-stage silence detection): the start beep,
//! capture through the [`Detector`] with a live level for the meter, and
//! the end double beep.

use pmt_config::schema::VoiceModeConfig;

use crate::device::{Clip, Microphone, Player, Sound};
use crate::pcm::{self, FRAME_MS, FRAME_SAMPLES, RATE};
use crate::vad::{Detector, End, Limits};

/// The start cue.
pub const START_BEEP_HZ: f64 = 880.0;
/// The end cue (played twice).
pub const END_BEEP_HZ: f64 = 660.0;

/// The input level while recording, for a meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// RMS of the last frames (0-32767).
    pub rms: f64,
    /// Audio recorded so far.
    pub elapsed_ms: u64,
    /// Speech has been confirmed.
    pub speech: bool,
}

/// Shows the level; called a few times a second.
pub type Meter = std::sync::Arc<dyn Fn(Level) + Send + Sync>;

/// A finished recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// The audio (16 kHz mono).
    pub samples: Vec<i16>,
    /// Why it ended.
    pub end: End,
}

impl Recording {
    /// Whether speech was confirmed (else nothing is transcribed).
    #[must_use]
    pub fn has_speech(&self) -> bool {
        self.end != End::NoSpeech
    }
}

/// Frames between two meter updates (100 ms).
const METER_EVERY: u64 = 5;

/// Records one utterance: beeps (unless `voice.beep_enabled = false`),
/// then captures until the detector ends it.
///
/// # Errors
///
/// The microphone cannot start, or stops before speech was heard.
pub async fn record(
    mic: &dyn Microphone,
    player: &dyn Player,
    config: &VoiceModeConfig,
    meter: &Meter,
) -> Result<Recording, String> {
    if config.beep_enabled {
        cue(player, "beep:start", START_BEEP_HZ, 1).await;
    }
    let recording = capture(mic, Limits::of(config), meter).await;
    if config.beep_enabled {
        cue(player, "beep:end", END_BEEP_HZ, 2).await;
    }
    recording
}

async fn cue(player: &dyn Player, label: &str, hz: f64, count: u32) {
    let clip = Clip {
        label: label.to_owned(),
        sound: Sound::Pcm {
            rate: RATE,
            samples: pcm::beep(hz, 120, count),
        },
    };
    if let Err(why) = player.play(clip).await {
        tracing::debug!(%why, "voice cue not played");
    }
}

async fn capture(mic: &dyn Microphone, limits: Limits, meter: &Meter) -> Result<Recording, String> {
    let mut device = mic.open().await?;
    let mut detector = Detector::new(limits);
    let mut samples = Vec::new();
    let mut pending: Vec<i16> = Vec::new();
    let mut frames = 0u64;
    while let Some(chunk) = device.read().await {
        pending.extend(chunk);
        let mut at = 0;
        while pending.len() - at >= FRAME_SAMPLES {
            let frame = &pending[at..at + FRAME_SAMPLES];
            at += FRAME_SAMPLES;
            samples.extend_from_slice(frame);
            let rms = pcm::rms(frame);
            let end = detector.feed(u64::from(FRAME_MS), rms);
            frames += 1;
            if frames.is_multiple_of(METER_EVERY) {
                meter(Level {
                    rms,
                    elapsed_ms: frames * u64::from(FRAME_MS),
                    speech: detector.heard_speech(),
                });
            }
            if let Some(end) = end {
                return Ok(Recording { samples, end });
            }
        }
        pending.drain(..at);
    }
    if detector.heard_speech() {
        return Ok(Recording {
            samples,
            end: End::Silence,
        });
    }
    Err("the microphone stopped delivering audio".to_owned())
}
