//! Reads what the `perf-sampler` binary measured (see its module docs for
//! why sampling happens in a separate, small process).

use std::path::Path;
use std::process::Command;

/// What one `perf-sampler` run measured.
#[derive(Debug, PartialEq)]
pub(crate) struct Samples {
    /// Wall time of each `--version` run, in milliseconds.
    pub(crate) startup_ms: Vec<f64>,
    /// Largest resident set of any run, when it can be attributed to the
    /// measured binary.
    pub(crate) peak_rss_bytes: Option<u64>,
}

/// Runs `sampler BINARY RUNS` and parses its output.
pub(crate) fn collect(sampler: &Path, binary: &Path, runs: usize) -> Result<Samples, String> {
    let output = Command::new(sampler)
        .arg(binary)
        .arg(runs.to_string())
        .output()
        .map_err(|e| format!("{}: {e}", sampler.display()))?;
    if !output.status.success() {
        return Err(format!(
            "perf-sampler failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn parse(text: &str) -> Result<Samples, String> {
    let mut samples = Samples {
        startup_ms: Vec::new(),
        peak_rss_bytes: None,
    };
    for line in text.lines() {
        let bad = || format!("perf-sampler output: unexpected line {line:?}");
        match line.split_once(' ').ok_or_else(bad)? {
            ("startup_ms", ms) => samples.startup_ms.push(ms.parse().map_err(|_| bad())?),
            ("peak_rss_bytes", "none") => samples.peak_rss_bytes = None,
            ("peak_rss_bytes", bytes) => {
                samples.peak_rss_bytes = Some(bytes.parse().map_err(|_| bad())?);
            }
            _ => return Err(bad()),
        }
    }
    if samples.startup_ms.is_empty() {
        return Err("perf-sampler output: no timed runs".to_owned());
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::{Samples, parse};

    #[test]
    fn parses_runs_and_peak() {
        let samples = parse("startup_ms 1.5\nstartup_ms 2\npeak_rss_bytes 4096\n").unwrap();
        assert_eq!(
            samples,
            Samples {
                startup_ms: vec![1.5, 2.0],
                peak_rss_bytes: Some(4096),
            }
        );
    }

    #[test]
    fn an_unmeasurable_peak_is_absent() {
        let samples = parse("startup_ms 1.5\npeak_rss_bytes none\n").unwrap();
        assert_eq!(samples.peak_rss_bytes, None);
    }

    #[test]
    fn garbage_and_empty_output_are_errors() {
        assert!(parse("startup_ms fast\n").is_err());
        assert!(parse("peak_rss_bytes 1\n").is_err());
    }
}
