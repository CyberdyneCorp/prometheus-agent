//! Budgets, the stored baseline and the comparison between them.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// p95 of `prometheus version` on the reference runner (spec: Startup time
/// budget). M0 measures `--version`, the same code path until `version`
/// lands in M1 group 4.
pub(crate) const STARTUP_P95_BUDGET_MS: f64 = 20.0;
/// p95 until `prometheus chat` with a mock provider shows its prompt (spec:
/// Startup time budget).
pub(crate) const CHAT_PROMPT_P95_BUDGET_MS: f64 = 150.0;
/// The only per-process peak in the spec (`prometheus chat -z`, Memory and
/// size budgets); the `--version` run is held to it too.
pub(crate) const PEAK_RSS_BUDGET_BYTES: u64 = 120 * MIB;
/// Stripped release binary (spec: Memory and size budgets).
pub(crate) const BINARY_BUDGET_BYTES: u64 = 80 * MIB;

/// Growth over the baseline that fails the gate (spec: 15%).
const REGRESSION_RATIO: f64 = 1.15;
/// Growth that never counts as a regression however large the ratio. A
/// `--version` run takes a few milliseconds, so 15% is well inside the
/// scheduling jitter of a shared runner; without these floors the gate
/// would fail at random.
const STARTUP_NOISE_MS: f64 = 1.0;
const RSS_NOISE_BYTES: u64 = MIB;
/// Size growth below 5% of the 80 MiB budget never counts. Size does not
/// jitter, but against a near-empty binary any real dependency is a large
/// percentage; the floor keeps the 15% rule meaningful once the binary has
/// real weight while letting small, deliberate additions through.
const BINARY_NOISE_BYTES: u64 = 4 * MIB;

/// One measurement of the stub binary, as stored in the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct Metrics {
    /// Median wall time of `--version`; the regression check uses it because
    /// it is far steadier than the p95.
    pub(crate) startup_median_ms: f64,
    /// p95 wall time of `--version`, checked against the absolute budget.
    pub(crate) startup_p95_ms: f64,
    /// Peak RSS of any `--version` run; absent where the OS does not report it.
    pub(crate) peak_rss_bytes: Option<u64>,
    /// Size of the stripped release binary.
    pub(crate) binary_bytes: u64,
    /// Median time until `prometheus chat` shows its prompt; the regression
    /// check uses it. Absent where it is not measured (non-Unix) and in
    /// baselines recorded before it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_prompt_median_ms: Option<f64>,
    /// p95 of the same, checked against the absolute budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_prompt_p95_ms: Option<f64>,
    /// Peak RSS of the chat starts and `chat -z` runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_peak_rss_bytes: Option<u64>,
}

/// `perf/baseline.json`: one entry per platform (`<os>-<arch>`), since a
/// time measured on one machine says nothing about another.
pub(crate) type Baseline = BTreeMap<String, Metrics>;

/// A failed check, printed one per line.
#[derive(Debug, PartialEq)]
pub(crate) struct Violation(String);

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks the absolute budgets, then the regression rule against `baseline`
/// when one is stored for this platform.
pub(crate) fn evaluate(current: &Metrics, baseline: Option<&Metrics>) -> Vec<Violation> {
    let rss = |bytes: Option<u64>| bytes.map(as_f64);
    let checks = [
        (
            "startup p95",
            Some(current.startup_p95_ms),
            STARTUP_P95_BUDGET_MS,
            ms as fn(f64) -> String,
        ),
        (
            "chat prompt p95",
            current.chat_prompt_p95_ms,
            CHAT_PROMPT_P95_BUDGET_MS,
            ms,
        ),
        (
            "peak RSS",
            rss(current.peak_rss_bytes),
            as_f64(PEAK_RSS_BUDGET_BYTES),
            mib_f64,
        ),
        (
            "chat peak RSS",
            rss(current.chat_peak_rss_bytes),
            as_f64(PEAK_RSS_BUDGET_BYTES),
            mib_f64,
        ),
        (
            "binary size",
            Some(as_f64(current.binary_bytes)),
            as_f64(BINARY_BUDGET_BYTES),
            mib_f64,
        ),
    ];
    let mut violations: Vec<Violation> = checks
        .into_iter()
        .filter_map(|(name, value, budget, show)| {
            let value = value.filter(|&value| value > budget)?;
            Some(Violation(format!(
                "{name} {} exceeds the budget of {}",
                show(value),
                show(budget)
            )))
        })
        .collect();
    if let Some(base) = baseline {
        violations.extend(regressions(current, base));
    }
    violations
}

/// The measurements the 15% rule compares: name, now, then, noise floor
/// and how to show a value. A measurement either side lacks is skipped.
type Compared = (
    &'static str,
    Option<f64>,
    Option<f64>,
    f64,
    fn(f64) -> String,
);

fn regressions(current: &Metrics, base: &Metrics) -> Vec<Violation> {
    let rss = |bytes: Option<u64>| bytes.map(as_f64);
    let compared: [Compared; 4] = [
        (
            "startup median",
            Some(current.startup_median_ms),
            Some(base.startup_median_ms),
            STARTUP_NOISE_MS,
            ms,
        ),
        (
            "chat prompt median",
            current.chat_prompt_median_ms,
            base.chat_prompt_median_ms,
            STARTUP_NOISE_MS,
            ms,
        ),
        (
            "peak RSS",
            rss(current.peak_rss_bytes),
            rss(base.peak_rss_bytes),
            as_f64(RSS_NOISE_BYTES),
            mib_f64,
        ),
        (
            "chat peak RSS",
            rss(current.chat_peak_rss_bytes),
            rss(base.chat_peak_rss_bytes),
            as_f64(RSS_NOISE_BYTES),
            mib_f64,
        ),
    ];
    let mut found: Vec<Violation> = compared
        .into_iter()
        .filter_map(|(name, now, then, noise, show)| regressed(name, now?, then?, noise, show))
        .collect();
    found.extend(regressed(
        "binary size",
        as_f64(current.binary_bytes),
        as_f64(base.binary_bytes),
        as_f64(BINARY_NOISE_BYTES),
        mib_f64,
    ));
    found
}

fn regressed(
    name: &str,
    now: f64,
    then: f64,
    noise: f64,
    show: fn(f64) -> String,
) -> Option<Violation> {
    if now <= then * REGRESSION_RATIO || now - then <= noise {
        return None;
    }
    let percent = (now / then - 1.0) * 100.0;
    Some(Violation(format!(
        "{name} regressed {percent:.0}% ({} -> {}), more than the allowed 15%",
        show(then),
        show(now)
    )))
}

/// Median and p95 (nearest rank) of the samples.
pub(crate) fn summarize(mut samples: Vec<f64>) -> (f64, f64) {
    samples.sort_by(f64::total_cmp);
    (percentile(&samples, 50), percentile(&samples, 95))
}

fn percentile(sorted: &[f64], pct: usize) -> f64 {
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[allow(clippy::cast_precision_loss, reason = "byte counts far below 2^52")]
fn as_f64(bytes: u64) -> f64 {
    bytes as f64
}

pub(crate) fn ms(value: f64) -> String {
    format!("{value:.2} ms")
}

pub(crate) fn mib(bytes: u64) -> String {
    mib_f64(as_f64(bytes))
}

fn mib_f64(bytes: f64) -> String {
    format!("{:.2} MiB", bytes / as_f64(MIB))
}

#[cfg(test)]
mod tests {
    use super::{MIB, Metrics, evaluate, summarize};

    const BASE: Metrics = Metrics {
        startup_median_ms: 2.0,
        startup_p95_ms: 3.0,
        peak_rss_bytes: Some(4 * MIB),
        binary_bytes: 2 * MIB,
        chat_prompt_median_ms: Some(40.0),
        chat_prompt_p95_ms: Some(50.0),
        chat_peak_rss_bytes: Some(10 * MIB),
    };

    #[test]
    fn baseline_itself_passes() {
        assert!(evaluate(&BASE, Some(&BASE)).is_empty());
        assert!(evaluate(&BASE, None).is_empty());
    }

    #[test]
    fn a_200ms_sleep_breaches_the_budget_and_the_baseline() {
        let slow = Metrics {
            startup_median_ms: 202.0,
            startup_p95_ms: 203.0,
            ..BASE
        };
        let messages: Vec<String> = evaluate(&slow, Some(&BASE))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].starts_with("startup p95 203.00 ms exceeds"));
        assert!(messages[1].starts_with("startup median regressed"));
    }

    #[test]
    fn growth_within_the_noise_floor_is_not_a_regression() {
        let jittery = Metrics {
            startup_median_ms: 2.9,
            peak_rss_bytes: Some(4 * MIB + MIB / 2),
            ..BASE
        };
        assert!(evaluate(&jittery, Some(&BASE)).is_empty());
    }

    #[test]
    fn binary_growth_under_the_size_floor_is_not_a_regression() {
        // The M1 dependencies took a 0.62 MiB binary to 1.21 MiB: +94%,
        // but well under the 4 MiB floor.
        let small = Metrics {
            binary_bytes: 634 * 1024,
            ..BASE
        };
        let doubled = Metrics {
            binary_bytes: 1239 * 1024,
            ..BASE
        };
        assert!(evaluate(&doubled, Some(&small)).is_empty());
    }

    #[test]
    fn binary_growth_over_15_percent_and_the_floor_fails() {
        let base = Metrics {
            binary_bytes: 20 * MIB,
            ..BASE
        };
        let bigger = Metrics {
            binary_bytes: 25 * MIB,
            ..BASE
        };
        let violations = evaluate(&bigger, Some(&base));
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0]
                .to_string()
                .starts_with("binary size regressed 25%")
        );
        // Over the floor but within 15% passes.
        let within = Metrics {
            binary_bytes: 30 * MIB,
            ..BASE
        };
        let base_large = Metrics {
            binary_bytes: 27 * MIB,
            ..BASE
        };
        assert!(evaluate(&within, Some(&base_large)).is_empty());
    }

    #[test]
    fn an_82_mib_binary_fails_the_size_budget() {
        let huge = Metrics {
            binary_bytes: 82 * MIB,
            ..BASE
        };
        let violations = evaluate(&huge, None);
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0]
                .to_string()
                .starts_with("binary size 82.00 MiB exceeds")
        );
    }

    /// Scenario-level budgets of `prometheus chat` (spec: Startup time
    /// budget, 150 ms to the prompt; Memory and size budgets, 120 MiB for
    /// `chat -z`).
    #[test]
    fn a_slow_or_heavy_chat_breaches_its_budgets() {
        let heavy = Metrics {
            chat_prompt_p95_ms: Some(151.0),
            chat_peak_rss_bytes: Some(121 * MIB),
            ..BASE
        };
        let messages: Vec<String> = evaluate(&heavy, None)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            messages,
            [
                "chat prompt p95 151.00 ms exceeds the budget of 150.00 ms",
                "chat peak RSS 121.00 MiB exceeds the budget of 120.00 MiB"
            ]
        );
    }

    #[test]
    fn a_chat_regression_fails_and_a_baseline_without_chat_skips_it() {
        let slower = Metrics {
            chat_prompt_median_ms: Some(48.0),
            ..BASE
        };
        let violations = evaluate(&slower, Some(&BASE));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0]
                .to_string()
                .starts_with("chat prompt median regressed 20%")
        );
        let old = Metrics {
            chat_prompt_median_ms: None,
            chat_prompt_p95_ms: None,
            chat_peak_rss_bytes: None,
            ..BASE
        };
        assert!(evaluate(&slower, Some(&old)).is_empty());
        let stored: Metrics = serde_json::from_str(
            r#"{"startup_median_ms":1.8,"startup_p95_ms":2.0,"peak_rss_bytes":null,"binary_bytes":1}"#,
        )
        .unwrap();
        assert_eq!(stored.chat_prompt_median_ms, None);
    }

    #[test]
    fn p95_is_the_nearest_rank() {
        let samples = (1..=30).map(f64::from).collect();
        assert_eq!(summarize(samples), (15.0, 29.0));
    }
}
