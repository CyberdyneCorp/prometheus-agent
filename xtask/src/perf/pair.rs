//! Measuring the head binary and, with `--base-binary`, the base branch's
//! binary in alternating rounds.
//!
//! CI used to measure the base in one step and the head ~20 minutes later,
//! after the head's release build. On a shared runner the machine's speed
//! drifts between the two: one run measured the head's `--version` 29%
//! slower than the base's, on code paths the change did not touch, and the
//! chat prompt median then crossed the 15% gate. Alternating the two
//! binaries in short rounds exposes both to the same drift, so their
//! difference is the change's.

use std::path::{Path, PathBuf};

use super::budget::{self, Metrics};
use super::{round2, sample};

/// Alternating rounds; every round runs `runs / ROUNDS` (at least one)
/// timed runs of each binary, the base first in even rounds.
const ROUNDS: usize = 4;

/// The samples of one binary, accumulated over rounds.
struct Subject {
    binary: PathBuf,
    chat_home: PathBuf,
    startup_ms: Vec<f64>,
    peak_rss_bytes: Option<u64>,
    chat_ms: Vec<f64>,
    chat_peak_rss_bytes: Option<u64>,
}

impl Subject {
    fn new(root: &Path, binary: &Path, label: &str) -> Self {
        let chat_home = root.join(format!("target/perf-chat-home-{label}"));
        let _ = std::fs::remove_dir_all(&chat_home);
        Self {
            binary: binary.to_path_buf(),
            chat_home,
            startup_ms: Vec::new(),
            peak_rss_bytes: None,
            chat_ms: Vec::new(),
            chat_peak_rss_bytes: None,
        }
    }

    /// One round: `runs` timed `--version` runs, then (Unix) `runs` chat
    /// starts in this subject's home.
    fn round(&mut self, sampler: &Path, runs: usize) -> Result<(), String> {
        let measured = sample::collect(sampler, &self.binary, runs, &[])?;
        self.startup_ms.extend(measured.startup_ms);
        self.peak_rss_bytes = self.peak_rss_bytes.max(measured.peak_rss_bytes);
        if cfg!(unix) {
            let mode = ["chat".as_ref(), self.chat_home.as_os_str()];
            let chat = sample::collect(sampler, &self.binary, runs, &mode)?;
            self.chat_ms.extend(chat.startup_ms);
            self.chat_peak_rss_bytes = self.chat_peak_rss_bytes.max(chat.peak_rss_bytes);
        }
        Ok(())
    }

    fn metrics(self) -> Result<Metrics, String> {
        let binary_bytes = std::fs::metadata(&self.binary)
            .map_err(|e| format!("{}: {e}", self.binary.display()))?
            .len();
        let (startup_median_ms, startup_p95_ms) = budget::summarize(self.startup_ms);
        let chat = (!self.chat_ms.is_empty()).then(|| budget::summarize(self.chat_ms));
        Ok(Metrics {
            startup_median_ms: round2(startup_median_ms),
            startup_p95_ms: round2(startup_p95_ms),
            peak_rss_bytes: self.peak_rss_bytes,
            binary_bytes,
            chat_prompt_median_ms: chat.map(|c| round2(c.0)),
            chat_prompt_p95_ms: chat.map(|c| round2(c.1)),
            chat_peak_rss_bytes: self.chat_peak_rss_bytes.filter(|_| chat.is_some()),
        })
    }
}

/// The metrics of `head` and, when given, of `base`, measured in
/// alternating rounds of about `runs` timed runs each in total.
pub(crate) fn measure(
    root: &Path,
    sampler: &Path,
    head: &Path,
    base: Option<&Path>,
    runs: usize,
) -> Result<(Metrics, Option<Metrics>), String> {
    let mut head = Subject::new(root, head, "head");
    let Some(base) = base else {
        head.round(sampler, runs)?;
        return Ok((head.metrics()?, None));
    };
    let mut base = Subject::new(root, base, "base");
    let per_round = runs.div_ceil(ROUNDS);
    for round in 0..ROUNDS {
        let (first, second) = if round % 2 == 0 {
            (&mut base, &mut head)
        } else {
            (&mut head, &mut base)
        };
        first.round(sampler, per_round)?;
        second.round(sampler, per_round)?;
    }
    Ok((head.metrics()?, Some(base.metrics()?)))
}
