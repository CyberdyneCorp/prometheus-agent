//! Barge-in (spec: voice-mode, Barge-in). While a reply generates and
//! plays, the microphone stays open: the first [`CALIBRATION_MS`] of a
//! turn, before anything is played, measure the quiet room, and speech
//! louder than `voice.barge_in_threshold_multiplier` times that floor for
//! [`TRIGGER_MS`] interrupts the reply. The floor is never measured from
//! playback, so the reply's own sound does not raise the bar.

use crate::device::Capture;
use crate::pcm::{self, FRAME_MS, FRAME_SAMPLES};

/// Room audio measured at turn start.
pub const CALIBRATION_MS: u64 = 200;
/// The lowest floor: a digitally silent room would otherwise make any
/// click a barge-in.
pub const MIN_FLOOR: f64 = 30.0;
/// Loud audio that must last before it counts as the user speaking; short
/// enough that playback stops well within 300 ms of the user starting.
pub const TRIGGER_MS: u64 = 160;
/// The note the next user message carries after a barge-in.
pub const INTERRUPTED_NOTE: &str =
    "[Your previous spoken reply was cut off: the user started talking over it.]";

/// The barge-in detector of one turn.
#[derive(Debug, Clone)]
pub struct Barge {
    multiplier: f64,
    calibrated_ms: u64,
    sum: f64,
    frames: u32,
    floor: Option<f64>,
    run_ms: u64,
}

impl Barge {
    /// A detector that triggers at `multiplier` times the room's level.
    #[must_use]
    pub fn new(multiplier: f64) -> Self {
        Self {
            multiplier,
            calibrated_ms: 0,
            sum: 0.0,
            frames: 0,
            floor: None,
            run_ms: 0,
        }
    }

    /// The room's level, once calibrated.
    #[must_use]
    pub fn floor(&self) -> Option<f64> {
        self.floor
    }

    /// The level speech must exceed, once calibrated.
    #[must_use]
    pub fn trigger(&self) -> Option<f64> {
        self.floor.map(|floor| floor * self.multiplier)
    }

    /// Feeds one frame of `ms` at level `rms`; `true` once the user has
    /// spoken over the trigger for [`TRIGGER_MS`].
    pub fn feed(&mut self, ms: u64, rms: f64) -> bool {
        let Some(trigger) = self.trigger() else {
            self.sum += rms;
            self.frames += 1;
            self.calibrated_ms += ms;
            if self.calibrated_ms >= CALIBRATION_MS {
                self.floor = Some((self.sum / f64::from(self.frames)).max(MIN_FLOOR));
            }
            return false;
        };
        self.run_ms = if rms > trigger { self.run_ms + ms } else { 0 };
        self.run_ms >= TRIGGER_MS
    }

    /// The trigger was reached and the user is still speaking.
    #[must_use]
    pub fn speaking(&self) -> bool {
        self.run_ms > 0
    }
}

/// A capture watched for barge-in.
pub struct Watch {
    device: Box<dyn Capture>,
    barge: Barge,
    pending: Vec<i16>,
    /// The frames of the current loud run: the start of the utterance.
    run: Vec<i16>,
    ended: bool,
}

impl Watch {
    /// Watches `device`; [`Watch::calibrate`] measures the room first.
    #[must_use]
    pub fn new(device: Box<dyn Capture>, multiplier: f64) -> Self {
        Self {
            device,
            barge: Barge::new(multiplier),
            pending: Vec::new(),
            run: Vec::new(),
            ended: false,
        }
    }

    /// Reads the quiet room until the floor is known.
    pub async fn calibrate(&mut self) {
        while self.barge.floor().is_none() && !self.ended {
            self.step().await;
        }
    }

    /// The detector.
    #[must_use]
    pub fn barge(&self) -> &Barge {
        &self.barge
    }

    /// Resolves when the user speaks over the reply; never when the device
    /// stops. Cancel safe between reads.
    pub async fn triggered(&mut self) {
        loop {
            if self.ended {
                std::future::pending::<()>().await;
            }
            if self.step().await {
                return;
            }
        }
    }

    /// Reads one chunk and feeds its frames; `true` on a trigger.
    async fn step(&mut self) -> bool {
        let Some(chunk) = self.device.read().await else {
            self.ended = true;
            return false;
        };
        self.pending.extend(chunk);
        let mut at = 0;
        let mut fired = false;
        while !fired && self.pending.len() - at >= FRAME_SAMPLES {
            let frame = &self.pending[at..at + FRAME_SAMPLES];
            at += FRAME_SAMPLES;
            let calibrating = self.barge.floor().is_none();
            fired = self.barge.feed(u64::from(FRAME_MS), pcm::rms(frame));
            if !calibrating && self.barge.speaking() {
                self.run.extend_from_slice(frame);
            } else {
                self.run.clear();
            }
        }
        self.pending.drain(..at);
        fired
    }

    /// The device and the audio heard since the user started speaking, to
    /// record the rest of the utterance.
    #[must_use]
    pub fn into_parts(self) -> (Box<dyn Capture>, Vec<i16>) {
        let mut heard = self.run;
        heard.extend(self.pending);
        (self.device, heard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(barge: &mut Barge, ms: u64, rms: f64) -> Option<u64> {
        (0..ms / 20).find_map(|n| barge.feed(20, rms).then_some((n + 1) * 20))
    }

    #[test]
    fn the_floor_comes_from_the_quiet_room_at_turn_start() {
        let mut barge = Barge::new(3.0);
        assert_eq!(feed(&mut barge, 200, 50.0), None);
        assert_eq!(barge.floor(), Some(50.0));
        assert_eq!(barge.trigger(), Some(150.0));
        // Room noise and quiet playback leakage stay under the trigger.
        assert_eq!(feed(&mut barge, 2_000, 140.0), None);
        // Speaking fires within 300 ms of starting.
        let fired_after = feed(&mut barge, 1_000, 2_000.0).unwrap();
        assert!(fired_after <= 300, "{fired_after} ms");
    }

    #[test]
    fn a_silent_room_has_a_minimum_floor_and_a_click_is_not_speech() {
        let mut barge = Barge::new(3.0);
        feed(&mut barge, 200, 0.0);
        assert_eq!(barge.floor(), Some(MIN_FLOOR));
        assert_eq!(feed(&mut barge, 60, 5_000.0), None);
        assert_eq!(feed(&mut barge, 100, 0.0), None);
        assert!(feed(&mut barge, 200, 5_000.0).is_some());
    }
}
