//! Wake engines and false-trigger control (spec: voice-mode, Wake engines
//! and false-trigger control). An engine hears 16 kHz frames and says when
//! a wake phrase was spoken; [`select`] picks one for `wake.engine`
//! (`auto`: the first this host supports, in [`AUTO_ORDER`]; a named
//! engine is never swapped for another), and the shared `wake.sensitivity`
//! is stricter-when-higher for every engine ([`Thresholds`]).

use pmt_config::schema::{WakeConfig, WakeEngine as Kind};

use crate::device::BoxFuture;

/// The engines `auto` tries, in order.
pub const AUTO_ORDER: [Kind; 3] = [Kind::Openwakeword, Kind::Sherpa, Kind::Porcupine];

/// The secret Porcupine needs.
pub const PORCUPINE_SECRET: &str = "PORCUPINE_ACCESS_KEY";

/// A wake phrase heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The phrase (as configured, lower case).
    pub phrase: String,
    /// The profile it wakes (`None`: the listening one).
    pub profile: Option<String>,
}

/// Detects wake phrases in a stream of frames.
pub trait WakeEngine: Send {
    /// Samples per frame it wants (16 kHz mono).
    fn frame_samples(&self) -> usize;
    /// Feeds one frame; `Some` when a wake phrase was confirmed.
    fn feed<'a>(&'a mut self, frame: &'a [i16])
    -> BoxFuture<'a, Result<Option<Detection>, String>>;
    /// Drops what it heard so far (called whenever listening resumes, so
    /// audio from before a pause can never fire).
    fn reset(&mut self) -> BoxFuture<'_, ()>;
}

/// Whether an engine can run here, or the requirement it misses.
pub trait Support {
    /// `Ok` when `kind` can run; else what is missing and how to get it.
    ///
    /// # Errors
    ///
    /// The engine's requirement error.
    fn check(&self, kind: Kind) -> Result<(), String>;
}

/// The engine `wake.engine` asks for on this host.
///
/// # Errors
///
/// A named engine that cannot run (its requirement error: no other
/// engine is tried), or `auto` with none available (every requirement).
pub fn select(config: &WakeConfig, support: &dyn Support) -> Result<Kind, String> {
    if config.engine != Kind::Auto {
        return support
            .check(config.engine)
            .map(|()| config.engine)
            .map_err(|why| format!("wake engine {} cannot run: {why}", config.engine.as_str()));
    }
    let mut missing = Vec::new();
    for kind in AUTO_ORDER {
        match support.check(kind) {
            Ok(()) => return Ok(kind),
            Err(why) => missing.push(format!("{}: {why}", kind.as_str())),
        }
    }
    Err(format!(
        "no wake engine can run here ({})",
        missing.join("; ")
    ))
}

/// What `wake.sensitivity` (0.0-1.0, stricter when higher) means to each
/// engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// The score an `openwakeword` frame must reach.
    pub openwakeword: f64,
    /// sherpa-onnx's `keywords_threshold` (0.05-0.45).
    pub sherpa: f64,
    /// Porcupine's own sensitivity, which runs the other way (higher is
    /// looser).
    pub porcupine: f64,
}

impl Thresholds {
    /// The thresholds of `sensitivity`.
    #[must_use]
    pub fn of(sensitivity: f64) -> Self {
        let s = sensitivity.clamp(0.0, 1.0);
        Self {
            openwakeword: s,
            sherpa: 0.4f64.mul_add(s, 0.05),
            porcupine: 1.0 - s,
        }
    }
}

/// `wake.confirmation_frames`: a real phrase holds the score high across
/// frames while a stray sound spikes one, so `openwakeword` fires only
/// after that many over-threshold frames in a row.
#[derive(Debug, Clone)]
pub struct Confirm {
    threshold: f64,
    needed: u32,
    streak: u32,
}

impl Confirm {
    /// Fires after `needed` frames scoring at least `threshold`.
    #[must_use]
    pub fn new(threshold: f64, needed: u32) -> Self {
        Self {
            threshold,
            needed: needed.max(1),
            streak: 0,
        }
    }

    /// One frame's score; `true` when the phrase is confirmed.
    pub fn score(&mut self, score: f64) -> bool {
        if score < self.threshold {
            self.streak = 0;
            return false;
        }
        self.streak += 1;
        if self.streak >= self.needed {
            self.streak = 0;
            return true;
        }
        false
    }

    /// Forgets the current streak.
    pub fn reset(&mut self) {
        self.streak = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Only(Vec<Kind>);

    impl Support for Only {
        fn check(&self, kind: Kind) -> Result<(), String> {
            if self.0.contains(&kind) {
                Ok(())
            } else if kind == Kind::Porcupine {
                Err(format!("the secret {PORCUPINE_SECRET} is not set"))
            } else {
                Err("not installed".to_owned())
            }
        }
    }

    #[test]
    fn one_spiking_frame_does_not_fire_but_three_in_a_row_do() {
        let mut confirm = Confirm::new(0.6, 3);
        assert!(!confirm.score(0.95), "a single 80 ms spike");
        assert!(!confirm.score(0.1));
        assert!(!confirm.score(0.7));
        assert!(!confirm.score(0.8));
        assert!(confirm.score(0.9), "the third frame in a row fires");
        assert!(!confirm.score(0.9), "and the streak starts over");
    }

    #[test]
    fn auto_takes_the_first_supported_engine() {
        let config = WakeConfig::default();
        assert_eq!(
            select(&config, &Only(vec![Kind::Sherpa, Kind::Porcupine])),
            Ok(Kind::Sherpa)
        );
        let none = select(&config, &Only(Vec::new())).unwrap_err();
        assert!(none.contains("openwakeword: not installed"), "{none}");
    }

    #[test]
    fn a_named_engine_is_never_swapped() {
        let config = WakeConfig {
            engine: Kind::Porcupine,
            ..WakeConfig::default()
        };
        let refused = select(&config, &Only(vec![Kind::Openwakeword, Kind::Sherpa])).unwrap_err();
        assert!(refused.contains(PORCUPINE_SECRET), "{refused}");
        assert!(
            refused.starts_with("wake engine porcupine cannot run"),
            "{refused}"
        );
    }

    #[test]
    fn higher_sensitivity_is_stricter_for_every_engine() {
        let loose = Thresholds::of(0.2);
        let strict = Thresholds::of(0.9);
        assert!(strict.openwakeword > loose.openwakeword);
        assert!(strict.sherpa > loose.sherpa);
        // Porcupine's knob runs the other way: lower is stricter.
        assert!(strict.porcupine < loose.porcupine);
    }
}
