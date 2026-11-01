//! When to compact and how much to keep (spec: context-compression,
//! Threshold from the main model's input budget; Small-window threshold
//! floor; Token-budgeted tail; Summary budget).
//!
//! Every figure derives from the MAIN model's window, so a switch to
//! another model re-derives all of them from the configured values (the
//! turn loop resolves them per request, from the current route).

use pmt_config::schema::{CompressionConfig, TailMode};

/// The trigger is never below this many tokens...
pub const MIN_TRIGGER: u64 = 64_000;
/// ...nor above this share of the input budget.
pub const MAX_TRIGGER_SHARE: f64 = 0.85;
/// Windows below this get the small-window floor.
pub const LARGE_WINDOW: u64 = 512_000;
/// The lowest effective threshold of a small window.
pub const SMALL_WINDOW_FLOOR: f64 = 0.75;
/// `lean` tail: this share of the window...
const LEAN_SHARE: f64 = 0.025;
/// ...clamped to these bounds...
const LEAN_MIN: u64 = 10_000;
const LEAN_MAX: u64 = 25_000;
/// ...and never above this share of the window (any mode).
const TAIL_MAX_SHARE: f64 = 0.20;
/// The summary gets this share of the summarized region...
const SUMMARY_SHARE: f64 = 0.20;
/// ...at least this many tokens...
const SUMMARY_MIN: u64 = 2_000;
/// ...and at most this share of the window (and `summary_max_tokens`).
const SUMMARY_MAX_SHARE: f64 = 0.05;

/// The main model's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelWindow {
    /// Context window in tokens.
    pub context_window: u64,
    /// Output tokens reserved for the answer (the model's output cap).
    pub reserved_output: u64,
}

impl ModelWindow {
    /// The tokens a request may use for input.
    #[must_use]
    pub const fn input_budget(self) -> u64 {
        self.context_window.saturating_sub(self.reserved_output)
    }
}

/// `floor(tokens * share)`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // token counts stay far below 2^52 and shares are in [0, 1]
fn share(tokens: u64, share: f64) -> u64 {
    (tokens as f64 * share).floor() as u64
}

/// The threshold in force for a `window`-token model: below 512,000
/// tokens at least 0.75 (a higher configured value wins).
#[must_use]
pub fn effective_threshold(configured: f64, window: u64) -> f64 {
    if window < LARGE_WINDOW {
        configured.max(SMALL_WINDOW_FLOOR)
    } else {
        configured
    }
}

/// The trigger in tokens: `floor(input_budget * effective_threshold)`,
/// floored at 64,000 but never above 85% of the input budget, lowered by
/// a non-zero `threshold_tokens` and by the compression model's window
/// when that is smaller.
#[must_use]
pub fn trigger_tokens(
    config: &CompressionConfig,
    main: ModelWindow,
    compressor_window: Option<u64>,
) -> u64 {
    let budget = main.input_budget();
    let threshold = effective_threshold(config.threshold, main.context_window);
    let mut trigger = share(budget, threshold)
        .max(MIN_TRIGGER)
        .min(share(budget, MAX_TRIGGER_SHARE));
    if config.threshold_tokens > 0 {
        trigger = trigger.min(config.threshold_tokens);
    }
    if let Some(window) = compressor_window {
        trigger = trigger.min(window);
    }
    trigger
}

/// The protected tail's token budget: `lean` 2.5% of the window clamped
/// to [10,000, 25,000], `ratio` `trigger * target_ratio`, never above 20%
/// of the window.
#[must_use]
pub fn tail_budget(config: &CompressionConfig, window: u64, trigger: u64) -> u64 {
    let wanted = match config.tail_mode {
        TailMode::Lean => share(window, LEAN_SHARE).clamp(LEAN_MIN, LEAN_MAX),
        TailMode::Ratio => share(trigger, config.target_ratio),
    };
    wanted.min(share(window, TAIL_MAX_SHARE))
}

/// The summary's token budget for a `region`-token middle: 20% of it, at
/// least 2,000 and at most `min(5% of the window, summary_max_tokens)`.
#[must_use]
pub fn summary_budget(config: &CompressionConfig, window: u64, region: u64) -> u64 {
    let cap = share(window, SUMMARY_MAX_SHARE).min(config.summary_max_tokens);
    share(region, SUMMARY_SHARE)
        .max(SUMMARY_MIN)
        .min(cap.max(SUMMARY_MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(threshold: f64, threshold_tokens: u64) -> CompressionConfig {
        CompressionConfig {
            threshold,
            threshold_tokens,
            ..CompressionConfig::default()
        }
    }

    fn window(context_window: u64, reserved_output: u64) -> ModelWindow {
        ModelWindow {
            context_window,
            reserved_output,
        }
    }

    #[test]
    fn trigger_table() {
        // (window, reserved, threshold, threshold_tokens, compressor, expected)
        let rows: [(u64, u64, f64, u64, Option<u64>, u64); 8] = [
            // Absolute cap: a 1M window at 0.50 capped by threshold_tokens.
            (1_000_000, 0, 0.50, 200_000, None, 200_000),
            // A 1M window at 0.50 with nothing else.
            (1_000_000, 0, 0.50, 0, None, 500_000),
            // Output reservation at an effective 0.80.
            (200_000, 65_536, 0.80, 0, None, 107_571),
            // A 200K window at 0.50 is lifted to 0.75.
            (200_000, 0, 0.50, 0, None, 150_000),
            // The 64K floor...
            (100_000, 0, 0.50, 0, None, 75_000),
            // ...never above 85% of a small input budget.
            (16_000, 0, 0.50, 0, None, 13_600),
            // A smaller compression model lowers it.
            (1_000_000, 0, 0.50, 0, Some(128_000), 128_000),
            // A small threshold_tokens wins over the 64K floor.
            (200_000, 0, 0.50, 3_000, None, 3_000),
        ];
        for (ctx, reserved, threshold, tokens, compressor, expected) in rows {
            assert_eq!(
                trigger_tokens(
                    &config(threshold, tokens),
                    window(ctx, reserved),
                    compressor
                ),
                expected,
                "{ctx} {reserved} {threshold} {tokens} {compressor:?}"
            );
        }
    }

    #[test]
    fn the_small_window_floor_follows_every_switch() {
        // 200K -> 1M: 0.75 -> 0.50; and back.
        assert!((effective_threshold(0.50, 200_000) - 0.75).abs() < f64::EPSILON);
        assert!((effective_threshold(0.50, 1_000_000) - 0.50).abs() < f64::EPSILON);
        assert!((effective_threshold(0.50, 200_000) - 0.75).abs() < f64::EPSILON);
        // A higher configured value wins.
        assert!((effective_threshold(0.90, 200_000) - 0.90).abs() < f64::EPSILON);
        assert!((effective_threshold(0.50, 511_999) - 0.75).abs() < f64::EPSILON);
        assert!((effective_threshold(0.50, 512_000) - 0.50).abs() < f64::EPSILON);
    }

    #[test]
    fn tail_budget_table() {
        let lean = CompressionConfig::default();
        let ratio = CompressionConfig {
            tail_mode: TailMode::Ratio,
            target_ratio: 0.20,
            ..CompressionConfig::default()
        };
        // Small local model: at most 20% of 16K.
        assert_eq!(tail_budget(&lean, 16_000, 13_600), 3_200);
        assert_eq!(tail_budget(&lean, 200_000, 150_000), 10_000);
        assert_eq!(tail_budget(&lean, 1_000_000, 500_000), 25_000);
        assert_eq!(tail_budget(&lean, 600_000, 300_000), 15_000);
        assert_eq!(tail_budget(&ratio, 200_000, 150_000), 30_000);
        assert_eq!(tail_budget(&ratio, 100_000, 90_000), 18_000);
    }

    #[test]
    fn summary_budget_table() {
        let config = CompressionConfig::default();
        // 100K middle on a 200K model.
        assert_eq!(summary_budget(&config, 200_000, 100_000), 10_000);
        assert_eq!(summary_budget(&config, 200_000, 20_000), 4_000);
        assert_eq!(summary_budget(&config, 200_000, 1_000), 2_000);
        // 5% of a 100K window caps below summary_max_tokens.
        assert_eq!(summary_budget(&config, 100_000, 100_000), 5_000);
        let capped = CompressionConfig {
            summary_max_tokens: 3_000,
            ..CompressionConfig::default()
        };
        assert_eq!(summary_budget(&capped, 1_000_000, 500_000), 3_000);
    }
}
