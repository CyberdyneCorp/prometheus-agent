//! Budgets, the stored baseline and the comparison between them.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// p95 of `prometheus version` on the reference runner (spec: Startup time
/// budget). M0 measures `--version`, the same code path until `version`
/// lands in M1 group 4.
pub(crate) const STARTUP_P95_BUDGET_MS: f64 = 20.0;
/// The only per-process peak in the spec (`prometheus chat -z`, Memory and
/// size budgets); until `chat` exists it bounds the `--version` run.
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
    let mut violations = Vec::new();
    let mut over_budget = |name: &str, value: String, budget: String| {
        violations.push(Violation(format!(
            "{name} {value} exceeds the budget of {budget}"
        )));
    };
    if current.startup_p95_ms > STARTUP_P95_BUDGET_MS {
        over_budget(
            "startup p95",
            ms(current.startup_p95_ms),
            ms(STARTUP_P95_BUDGET_MS),
        );
    }
    if let Some(rss) = current
        .peak_rss_bytes
        .filter(|&rss| rss > PEAK_RSS_BUDGET_BYTES)
    {
        over_budget("peak RSS", mib(rss), mib(PEAK_RSS_BUDGET_BYTES));
    }
    if current.binary_bytes > BINARY_BUDGET_BYTES {
        over_budget(
            "binary size",
            mib(current.binary_bytes),
            mib(BINARY_BUDGET_BYTES),
        );
    }
    if let Some(base) = baseline {
        violations.extend(regressions(current, base));
    }
    violations
}

fn regressions(current: &Metrics, base: &Metrics) -> Vec<Violation> {
    let mut found = Vec::new();
    let mut check = |name: &str, now: f64, then: f64, noise: f64, show: fn(f64) -> String| {
        if now > then * REGRESSION_RATIO && now - then > noise {
            let percent = (now / then - 1.0) * 100.0;
            found.push(Violation(format!(
                "{name} regressed {percent:.0}% ({} -> {}), more than the allowed 15%",
                show(then),
                show(now)
            )));
        }
    };
    check(
        "startup median",
        current.startup_median_ms,
        base.startup_median_ms,
        STARTUP_NOISE_MS,
        ms,
    );
    if let (Some(now), Some(then)) = (current.peak_rss_bytes, base.peak_rss_bytes) {
        check(
            "peak RSS",
            as_f64(now),
            as_f64(then),
            as_f64(RSS_NOISE_BYTES),
            mib_f64,
        );
    }
    check(
        "binary size",
        as_f64(current.binary_bytes),
        as_f64(base.binary_bytes),
        0.0,
        mib_f64,
    );
    found
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
    fn binary_growth_over_15_percent_fails() {
        let bigger = Metrics {
            binary_bytes: 2 * MIB + MIB / 2,
            ..BASE
        };
        let violations = evaluate(&bigger, Some(&BASE));
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0]
                .to_string()
                .starts_with("binary size regressed 25%")
        );
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

    #[test]
    fn p95_is_the_nearest_rank() {
        let samples = (1..=30).map(f64::from).collect();
        assert_eq!(summarize(samples), (15.0, 29.0));
    }
}
