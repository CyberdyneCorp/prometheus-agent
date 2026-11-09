//! Two-stage silence detection (spec: voice-mode, Two-stage silence
//! detection). Stage one confirms speech: the level stays above the
//! threshold for `speech_confirm_ms`, dips shorter than [`DIP_TOLERANCE_MS`]
//! not breaking the run. Stage two, once speech is confirmed, ends the
//! recording after `silence_duration_ms` of continuous quiet. Without
//! confirmed speech the recording ends at `no_speech_timeout_secs`, and
//! every recording ends at `max_recording_secs`.
//!
//! Time is audio time (the frames fed), never the wall clock, so a
//! recorded fixture is judged exactly as live capture would be.

use pmt_config::schema::VoiceModeConfig;

/// Quiet shorter than this does not break a run of speech being confirmed.
pub const DIP_TOLERANCE_MS: u64 = 200;

/// The detection settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Speech level (RMS, 0-32767).
    pub threshold: f64,
    /// Above-threshold run that confirms speech.
    pub confirm_ms: u64,
    /// Quiet after speech that ends the recording.
    pub silence_ms: u64,
    /// No confirmed speech by then: nothing was said.
    pub no_speech_ms: u64,
    /// The longest recording.
    pub max_ms: u64,
}

impl Limits {
    /// The limits `[voice]` sets.
    #[must_use]
    pub fn of(config: &VoiceModeConfig) -> Self {
        Self {
            threshold: f64::from(config.silence_threshold),
            confirm_ms: config.speech_confirm_ms,
            silence_ms: config.silence_duration_ms,
            no_speech_ms: config.no_speech_timeout_secs.saturating_mul(1_000),
            max_ms: config.max_recording_secs.saturating_mul(1_000),
        }
    }
}

/// Why a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Speech, then enough quiet.
    Silence,
    /// No speech was confirmed in time: nothing to transcribe.
    NoSpeech,
    /// `max_recording_secs` reached.
    MaxLength,
}

/// The detector of one recording.
#[derive(Debug, Clone)]
pub struct Detector {
    limits: Limits,
    elapsed_ms: u64,
    /// The current above-threshold run (with tolerated dips).
    run_ms: u64,
    /// Quiet inside the current run.
    dip_ms: u64,
    confirmed: bool,
    /// Continuous quiet since confirmed speech.
    quiet_ms: u64,
}

impl Detector {
    /// A detector at the start of a recording.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            elapsed_ms: 0,
            run_ms: 0,
            dip_ms: 0,
            confirmed: false,
            quiet_ms: 0,
        }
    }

    /// Whether speech has been confirmed.
    #[must_use]
    pub fn heard_speech(&self) -> bool {
        self.confirmed
    }

    /// Feeds one frame of `ms` milliseconds at level `rms`; `Some` once the
    /// recording is over.
    pub fn feed(&mut self, ms: u64, rms: f64) -> Option<End> {
        self.elapsed_ms += ms;
        let loud = rms > self.limits.threshold;
        if self.confirmed {
            self.quiet_ms = if loud { 0 } else { self.quiet_ms + ms };
            if self.quiet_ms >= self.limits.silence_ms {
                return Some(End::Silence);
            }
        } else {
            self.confirm(ms, loud);
            if !self.confirmed && self.elapsed_ms >= self.limits.no_speech_ms {
                return Some(End::NoSpeech);
            }
        }
        let end = if self.confirmed {
            End::MaxLength
        } else {
            End::NoSpeech
        };
        (self.elapsed_ms >= self.limits.max_ms).then_some(end)
    }

    /// Stage one: grow or break the run that confirms speech.
    fn confirm(&mut self, ms: u64, loud: bool) {
        if loud {
            self.run_ms += self.dip_ms + ms;
            self.dip_ms = 0;
        } else if self.run_ms > 0 {
            self.dip_ms += ms;
            if self.dip_ms >= DIP_TOLERANCE_MS {
                self.run_ms = 0;
                self.dip_ms = 0;
            }
        }
        if self.run_ms >= self.limits.confirm_ms {
            self.confirmed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> Detector {
        Detector::new(Limits::of(&VoiceModeConfig::default()))
    }

    /// Feeds `ms` of audio at `level` in 20 ms frames.
    fn feed(d: &mut Detector, ms: u64, level: f64) -> Option<End> {
        (0..ms / 20).find_map(|_| d.feed(20, level))
    }

    #[test]
    fn a_short_click_does_not_confirm_speech() {
        let mut d = detector();
        assert_eq!(feed(&mut d, 100, 5_000.0), None);
        assert_eq!(feed(&mut d, 400, 0.0), None);
        assert!(!d.heard_speech());
    }

    #[test]
    fn dips_under_200_ms_do_not_break_confirmation() {
        let mut d = detector();
        feed(&mut d, 160, 1_000.0);
        feed(&mut d, 120, 10.0);
        feed(&mut d, 60, 1_000.0);
        assert!(d.heard_speech());
        let mut d = detector();
        feed(&mut d, 160, 1_000.0);
        feed(&mut d, 200, 10.0);
        feed(&mut d, 60, 1_000.0);
        assert!(!d.heard_speech());
    }

    #[test]
    fn three_seconds_of_quiet_after_speech_end_the_recording() {
        let mut d = detector();
        assert_eq!(feed(&mut d, 1_000, 900.0), None);
        assert_eq!(feed(&mut d, 2_980, 50.0), None);
        assert_eq!(d.feed(20, 50.0), Some(End::Silence));
    }

    #[test]
    fn the_longest_recording_is_cut() {
        let config = VoiceModeConfig {
            max_recording_secs: 5,
            ..VoiceModeConfig::default()
        };
        let mut d = Detector::new(Limits::of(&config));
        assert_eq!(feed(&mut d, 6_000, 900.0), Some(End::MaxLength));
        assert_eq!(d.elapsed_ms, 5_000);
    }
}
