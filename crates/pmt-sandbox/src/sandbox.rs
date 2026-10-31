//! The per-session sandbox: probes the host once, applies the downgrade rule
//! and builds confined child commands (spec: security-boundaries, Hardening
//! downgrades are never silent; execution-environments, Local backend runs
//! under the OS sandbox).

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use pmt_config::schema::{DegradedFeature, SandboxConfig, SandboxMode};
use pmt_core::error::{Error, codes};

use crate::platform;
use crate::policy::Policy;
use crate::report::{Enforcement, Gap, StrengthReport, allow_hint};

/// Construction options; the defaults suit production.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The executable that starts confined children on Windows (it is run
    /// with [`crate::LAUNCH_ARG`] first; see [`crate::launcher`]). Default:
    /// the current executable. Ignored elsewhere.
    pub launcher: Option<PathBuf>,
    /// Use this report instead of probing the host (tests, doctor previews).
    pub report: Option<StrengthReport>,
}

/// A session's OS sandbox for one effective capability set.
#[derive(Debug)]
pub struct Sandbox {
    policy: Policy,
    report: StrengthReport,
    refusal: Option<Gap>,
    downgrades: Vec<Gap>,
    prepared: Option<platform::Prepared>,
}

impl Sandbox {
    /// Probes this host for `policy` under `config`, logging a WARN for every
    /// allowed downgrade.
    ///
    /// # Errors
    ///
    /// `sandbox.setup` when the platform sandbox cannot be prepared.
    pub fn new(policy: Policy, config: &SandboxConfig) -> Result<Self, Error> {
        Self::with_options(policy, config, Options::default())
    }

    /// [`Sandbox::new`] with explicit options.
    ///
    /// # Errors
    ///
    /// `sandbox.setup` when the platform sandbox cannot be prepared.
    pub fn with_options(
        policy: Policy,
        config: &SandboxConfig,
        options: Options,
    ) -> Result<Self, Error> {
        let report = match (config.mode, options.report) {
            (SandboxMode::Off, _) => StrengthReport::none(
                "off",
                &Gap::new(DegradedFeature::All, "security.sandbox.mode is off"),
            ),
            (SandboxMode::Enforce, Some(report)) => report,
            (SandboxMode::Enforce, None) => platform::probe(&policy),
        };
        let gaps = report.gaps();
        let refusal = gaps
            .iter()
            .find(|gap| !gap.is_allowed(&config.allow_degraded))
            .map(|gap| (*gap).clone());
        let downgrades: Vec<Gap> = gaps.into_iter().cloned().collect();
        let prepared = if refusal.is_none() && report.enforcement() != Enforcement::None {
            let launcher = options.launcher.or_else(|| std::env::current_exe().ok());
            Some(platform::prepare(&policy, &report, launcher)?)
        } else {
            None
        };
        if refusal.is_none() {
            for gap in &downgrades {
                tracing::warn!(
                    feature = gap.hardening.as_str(),
                    "sandbox degraded: {}; allowed by security.sandbox.allow_degraded",
                    gap.reason
                );
            }
        }
        Ok(Self {
            policy,
            report,
            refusal,
            downgrades,
            prepared,
        })
    }

    /// Enforcement per feature on this host.
    #[must_use]
    pub fn report(&self) -> &StrengthReport {
        &self.report
    }

    /// The policy being enforced.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Every hardening feature below the platform's full level (allowed or
    /// not); a session emits one `security.downgrade` event per entry.
    #[must_use]
    pub fn downgrades(&self) -> &[Gap] {
        &self.downgrades
    }

    /// The marker every session banner shows while degraded, for example
    /// `sandbox: partial:landlock_net`.
    #[must_use]
    pub fn banner_marker(&self) -> Option<String> {
        match self.report.enforcement() {
            Enforcement::Full => None,
            other => Some(format!("sandbox: {other}")),
        }
    }

    /// Whether commands may run.
    ///
    /// # Errors
    ///
    /// `sandbox.unavailable` (exit 77) naming the first missing feature that
    /// `security.sandbox.allow_degraded` does not list.
    pub fn check(&self) -> Result<(), Error> {
        match &self.refusal {
            None => Ok(()),
            Some(gap) => Err(Error::new(
                &codes::SANDBOX_UNAVAILABLE,
                format!(
                    "sandbox unavailable: {}; {}",
                    gap.reason,
                    allow_hint(gap.hardening)
                ),
            )),
        }
    }

    /// A command for `program` that runs confined. Add arguments, working
    /// directory, environment and stdio as usual (the environment is NOT
    /// cleared here; the terminal backend builds it from an allowlist).
    ///
    /// # Errors
    ///
    /// `sandbox.unavailable` as in [`Sandbox::check`]; `sandbox.setup` when
    /// the confinement for this child cannot be built.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Result<Command, Error> {
        self.check()?;
        match &self.prepared {
            Some(prepared) => prepared.command(&self.policy, program.as_ref()),
            None => Ok(Command::new(program)),
        }
    }
}

/// A `sandbox.setup` error.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn setup_error(what: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::SANDBOX_SETUP,
        format!("sandbox setup failed: {what}"),
    )
}
