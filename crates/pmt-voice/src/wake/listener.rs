//! The armed wake listener (spec: voice-mode, Wake word listener;
//! Recording retention and privacy; Wake and voice diagnostics).
//!
//! [`wait`] feeds the microphone to the engine until it hears the wake
//! phrase. The audio waiting for the engine is a [`Ring`] of at most
//! [`MAX_BUFFER_MS`]; nothing of it is ever written anywhere, and it is
//! dropped on detection. [`answer`] is what follows a detection: record
//! one utterance with voice-mode silence detection, submit it and speak
//! the reply; the surface then resumes listening (the engine is reset so
//! audio from before the pause cannot fire again). A microphone that
//! delivers only digital silence for [`SILENT_SECS`] is reported.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::cancel::CancelToken;

use super::engine::{Detection, WakeEngine};
use crate::converse::{self, Heard, Host, Kit};
use crate::device::Capture;
use crate::pcm::RATE;
use crate::speaker::Mouth;

/// The most audio kept in memory while listening.
pub const MAX_BUFFER_MS: usize = 2_000;
/// Digital silence after which the microphone is reported.
pub const SILENT_SECS: u64 = 10;
/// The report.
pub const SILENT_MIC: &str = "microphone delivers only silence";

/// How to let this process use the microphone, for the silence report.
#[must_use]
pub fn permission_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "allow the terminal (or the Prometheus app) under System Settings > Privacy & Security > Microphone, then restart it"
    } else if cfg!(windows) {
        "turn on 'Let desktop apps access your microphone' under Settings > Privacy & security > Microphone"
    } else {
        "check that the input source is not muted (pactl list sources) and that this user may record (the audio group)"
    }
}

/// Rolling audio waiting for the engine, never more than
/// [`MAX_BUFFER_MS`].
#[derive(Debug, Default)]
pub struct Ring {
    samples: VecDeque<i16>,
}

impl Ring {
    /// The most samples it holds.
    pub const CAPACITY: usize = RATE as usize * MAX_BUFFER_MS / 1_000;

    /// Adds `samples`, dropping the oldest beyond the capacity.
    pub fn push(&mut self, samples: &[i16]) {
        self.samples.extend(samples);
        let over = self.samples.len().saturating_sub(Self::CAPACITY);
        self.samples.drain(..over);
    }

    /// The next `n` samples, when that many are waiting.
    pub fn frame(&mut self, n: usize) -> Option<Vec<i16>> {
        (n > 0 && self.samples.len() >= n).then(|| self.samples.drain(..n).collect())
    }

    /// Samples held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// What the listener reports (`/wake status`, the desktop ear).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    /// The microphone has delivered only digital silence for
    /// [`SILENT_SECS`].
    pub silent: bool,
    /// The last error (the engine or the device failed).
    pub error: Option<String>,
}

/// The listener's health, shared with the surface.
pub type SharedHealth = Arc<Mutex<Health>>;

/// Counts digital silence.
#[derive(Debug, Default)]
struct Silence {
    zero_samples: u64,
}

impl Silence {
    /// `true` once the silence has lasted [`SILENT_SECS`].
    fn feed(&mut self, samples: &[i16]) -> bool {
        if samples.iter().all(|s| *s == 0) {
            self.zero_samples += samples.len() as u64;
        } else {
            self.zero_samples = 0;
        }
        self.zero_samples >= SILENT_SECS * u64::from(RATE)
    }
}

/// Listens on `capture` until `engine` hears a wake phrase.
///
/// # Errors
///
/// The device stopped or the engine failed.
pub async fn wait(
    capture: &mut dyn Capture,
    engine: &mut dyn WakeEngine,
    health: &SharedHealth,
) -> Result<Detection, String> {
    let mut ring = Ring::default();
    let mut silence = Silence::default();
    loop {
        let Some(chunk) = capture.read().await else {
            return Err("the microphone stopped delivering audio".to_owned());
        };
        let silent = silence.feed(&chunk);
        {
            let mut health = health.lock().unwrap_or_else(PoisonError::into_inner);
            if health.silent != silent {
                health.silent = silent;
                if silent {
                    // Reported by `/wake status`, not printed over the
                    // prompt.
                    tracing::debug!("{SILENT_MIC}: {}", permission_hint());
                }
            }
        }
        ring.push(&chunk);
        while let Some(frame) = ring.frame(engine.frame_samples()) {
            if let Some(detection) = engine.feed(&frame).await? {
                // Pre-wake audio is dropped, never kept.
                ring.clear();
                return Ok(detection);
            }
        }
    }
}

/// What answering a wake did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// The request was submitted and its reply spoken.
    Replied(String),
    /// Nothing was said after the wake phrase (or a stop phrase).
    Nothing,
}

/// Records the command after a wake, submits it and speaks the reply.
///
/// # Errors
///
/// Recording or transcription failed.
pub async fn answer<H: Host>(host: &mut H, kit: &Kit) -> Result<Answered, String> {
    let Heard::Speech(text) = converse::listen(kit).await? else {
        return Ok(Answered::Nothing);
    };
    host.heard(&text);
    let mouth = kit
        .voice
        .as_ref()
        .map(|voice| Mouth::start(Arc::clone(voice), Arc::clone(&kit.player)));
    host.turn(text.clone(), mouth.clone(), CancelToken::new())
        .await;
    if let Some(mouth) = mouth {
        mouth.finish().await;
    }
    Ok(Answered::Replied(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_never_holds_more_than_two_seconds() {
        let mut ring = Ring::default();
        for _ in 0..50 {
            ring.push(&[1; 1_600]);
        }
        assert_eq!(ring.len(), 32_000);
        assert_eq!(ring.frame(1_280).map(|f| f.len()), Some(1_280));
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.frame(1_280), None);
    }

    #[test]
    fn ten_seconds_of_zeros_is_a_silent_microphone() {
        let mut silence = Silence::default();
        for _ in 0..99 {
            assert!(!silence.feed(&[0; 1_600]));
        }
        assert!(silence.feed(&[0; 1_600]));
        assert!(!silence.feed(&[0, 3, 0]), "any signal resets it");
    }
}
