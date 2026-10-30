//! Platforms without an OS sandbox backend (Android, the BSDs): every
//! feature is unavailable, so commands run only when
//! `security.sandbox.allow_degraded` contains `all`.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use pmt_config::schema::DegradedFeature;
use pmt_core::error::Error;

use crate::policy::Policy;
use crate::report::{Gap, StrengthReport};

/// Nothing to prepare.
#[derive(Debug)]
pub(crate) struct Prepared;

pub(crate) fn probe(_policy: &Policy) -> StrengthReport {
    let gap = Gap::new(
        DegradedFeature::All,
        format!("no OS sandbox backend for {}", std::env::consts::OS),
    );
    StrengthReport::none("none", &gap)
}

#[allow(clippy::unnecessary_wraps, reason = "one signature for every platform")]
pub(crate) fn prepare(
    _policy: &Policy,
    _report: &StrengthReport,
    _launcher: Option<PathBuf>,
) -> Result<Prepared, Error> {
    Ok(Prepared)
}

impl Prepared {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "one signature for every platform; Linux can fail"
    )]
    pub(crate) fn command(&self, _policy: &Policy, program: &OsStr) -> Result<Command, Error> {
        Ok(Command::new(program))
    }
}
