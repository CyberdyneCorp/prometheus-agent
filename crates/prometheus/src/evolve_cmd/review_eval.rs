//! `evolve review <proposal> --eval <suite>` (spec: harness-evolution,
//! Human-only evaluation of proposals): the suite runs once on the base
//! harness and once on the proposed one, and the results are shown to
//! the user only. They are written outside `<profile>/evolve/`
//! (`<profile>/eval-results/`) with the verdict canary, so the tripwire
//! rejects them wherever they might turn up, and they are never stored in
//! a proposal, a harness version or an experience entry.
//!
//! The suites this build bundles: `smoke`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::harness::Tree;
use pmt_core::error::{Error, codes};
use pmt_evolve::engine::tripwire::VERDICT_CANARY;
use serde_json::{Value, json};

use crate::smoke_suite::runner::{self, Report as SuiteReport};

/// Both runs of a review.
#[derive(Debug, Clone)]
pub(super) struct Report {
    suite: String,
    base: SuiteReport,
    proposed: SuiteReport,
    /// The verdict file.
    pub(super) path: PathBuf,
}

fn side(report: &SuiteReport) -> Value {
    let ran = report.passed.len() + report.failed.len();
    let rate = if ran == 0 {
        0.0
    } else {
        f64::from(u32::try_from(report.passed.len()).unwrap_or(0))
            / f64::from(u32::try_from(ran).unwrap_or(1))
    };
    json!({
        "passed_count": report.passed.len(),
        "pass_rate": rate,
        "failed": report.failed.iter().map(|(id, why)| json!({"task": id, "why": why})).collect::<Vec<_>>(),
        "skipped": report.skipped,
    })
}

impl Report {
    /// The verdict as JSON (what the file holds).
    #[must_use]
    pub(super) fn json(&self) -> Value {
        json!({
            "canary": VERDICT_CANARY,
            "suite": self.suite,
            "base": side(&self.base),
            "proposed": side(&self.proposed),
            "path": self.path,
        })
    }

    /// The lines `evolve review` prints.
    #[must_use]
    pub(super) fn render(&self) -> String {
        let count = |r: &SuiteReport| (r.passed.len(), r.passed.len() + r.failed.len());
        let (bp, bt) = count(&self.base);
        let (pp, pt) = count(&self.proposed);
        let mut text = format!(
            "\neval {} (for you only; never shown to evolution):\n  base:     {bp} of {bt} tasks passed\n  proposed: {pp} of {pt} tasks passed\n",
            self.suite
        );
        for (id, why) in &self.proposed.failed {
            let _ = writeln!(text, "  proposed FAILED {id}: {why}");
        }
        let _ = writeln!(text, "  results: {}", self.path.display());
        text
    }
}

/// Runs `suite` on `base` and `proposed`.
///
/// # Errors
///
/// `cli.usage` for a suite this build does not bundle; I/O errors.
pub(super) fn run(
    scope: &ProfileScope,
    proposal: &str,
    suite: &str,
    base: &Tree,
    proposed: &Tree,
) -> Result<Report, Error> {
    if suite != "smoke" {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "eval suite '{suite}' is not bundled with this build; --eval smoke runs the bundled suite"
            ),
        ));
    }
    let bin = std::env::current_exe()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot find this binary: {e}")))?;
    let stamp = jiff::Timestamp::now().as_second();
    let out = scope
        .dir()
        .join("eval-results")
        .join(format!("review-{proposal}-{stamp}"));
    let base_report = runner::suite(&bin, &out.join("base"), "", Some(base), &[], |_| {});
    let proposed_report =
        runner::suite(&bin, &out.join("proposed"), "", Some(proposed), &[], |_| {});
    let report = Report {
        suite: suite.to_owned(),
        base: base_report,
        proposed: proposed_report,
        path: out.join("verdict.json"),
    };
    // The scratch homes go; the verdict stays for the user.
    for scratch in ["base", "proposed"] {
        let _ = std::fs::remove_dir_all(out.join(scratch));
    }
    write(&report.path, &report.json())?;
    Ok(report)
}

fn write(path: &Path, value: &Value) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        pmt_core::private_fs::create_dir(parent)
            .map_err(|e| Error::io(&codes::OUTPUT_IO, parent, &e))?;
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    std::fs::write(path, text).map_err(|e| Error::io(&codes::OUTPUT_IO, path, &e))
}
