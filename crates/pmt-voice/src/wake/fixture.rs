//! A stand-in engine for tests and the debug-build audio seam: it "hears"
//! the wake phrase in the first stretch of speech (300 ms over the voice
//! threshold) and reports the detection it was given, so recorded audio
//! drives the listener the way a real engine would.

use super::engine::{Detection, WakeEngine};
use crate::device::BoxFuture;
use crate::pcm;

/// Loud audio that counts as the wake phrase.
const SPEECH_MS: u64 = 300;
/// The level of speech.
const LEVEL: f64 = 500.0;

/// Fires `detection` on the first 300 ms of speech after each reset.
#[derive(Debug, Clone)]
pub struct FixtureEngine {
    detection: Detection,
    loud_ms: u64,
}

impl FixtureEngine {
    /// An engine that reports `detection`.
    #[must_use]
    pub fn new(detection: Detection) -> Self {
        Self {
            detection,
            loud_ms: 0,
        }
    }
}

impl WakeEngine for FixtureEngine {
    fn frame_samples(&self) -> usize {
        1_280
    }

    fn feed<'a>(
        &'a mut self,
        frame: &'a [i16],
    ) -> BoxFuture<'a, Result<Option<Detection>, String>> {
        Box::pin(async move {
            self.loud_ms = if pcm::rms(frame) > LEVEL {
                self.loud_ms + 80
            } else {
                0
            };
            Ok((self.loud_ms >= SPEECH_MS).then(|| {
                self.loud_ms = 0;
                self.detection.clone()
            }))
        })
    }

    fn reset(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.loud_ms = 0;
        })
    }
}
