//! The golden-recording regression gate (spec: replay-and-evaluation,
//! Golden recording regression gate): every `*.pmtrec` of a directory
//! replayed in `strict` mode; any divergence, or a trajectory that no
//! longer matches, fails the gate naming the recording.

use std::path::{Path, PathBuf};

use pmt_core::env::LaunchEnv;
use pmt_eval::recording::{EXTENSION, Recording};

use super::run::{Settings, run};
use super::{Mode, OnDivergence};

/// The recordings under `dir`, sorted.
///
/// # Errors
///
/// `dir` cannot be read.
pub fn goldens(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == EXTENSION))
        .collect();
    found.sort();
    Ok(found)
}

/// One failed golden: its file name and what its replay reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The recording's file name.
    pub name: String,
    /// The replay's report (the divergence and its diff), or its error.
    pub report: String,
}

/// Replays every golden of `dir` strictly, each in its own directory
/// under `scratch`; returns the ones that failed and how many ran.
///
/// # Errors
///
/// `dir` cannot be read.
pub fn check(
    dir: &Path,
    scratch: &Path,
    launch: &LaunchEnv,
) -> std::io::Result<(Vec<Failure>, usize)> {
    let goldens = goldens(dir)?;
    let mut failures = Vec::new();
    for path in &goldens {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let out = scratch.join(name.trim_end_matches(&format!(".{EXTENSION}")));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out)?;
        let report = Recording::open(path).and_then(|recording| {
            run(
                &recording,
                &Settings {
                    mode: Mode::Strict,
                    on_divergence: OnDivergence::Stop,
                    out: &out,
                    launch,
                    secrets_from: None,
                },
            )
        });
        let failed = match report {
            Ok(report) if report.matched() => None,
            Ok(report) => Some(report.text()),
            Err(err) => Some(format!("{}: {}\n", err.code().code, err.message())),
        };
        if let Some(report) = failed {
            failures.push(Failure { name, report });
        }
    }
    Ok((failures, goldens.len()))
}
