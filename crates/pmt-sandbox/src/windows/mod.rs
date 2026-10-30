//! Windows: an `AppContainer` (filesystem through ACL entries for its SID,
//! network through capabilities) inside a Job Object, started by the
//! launcher process ([`crate::launcher`]).
//!
//! Full level on Windows: `fs`, `net` and `process` `enforced`; `syscalls`
//! is `partial` (the `AppContainer` token gates object access, there is no
//! system-call filter) and that is the platform's maximum. `net` is
//! `enforced` when the policy allows all public connections
//! (`internetClient`) or none; capabilities cannot filter ports or hosts, so
//! a port or host list is `partial`. Listening is never granted (loopback
//! needs a per-container exemption), which is narrower than any grant.

mod args;
mod container;
pub(crate) mod launch;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use pmt_config::schema::DegradedFeature;
use pmt_core::error::Error;

pub(crate) use args::LaunchSpec;
use container::{Access, AppContainer};

use crate::policy::{Policy, Ports};
use crate::report::{Feature, FeatureReport, Gap, Level, StrengthReport};
use crate::sandbox::setup_error;

const PLATFORM: &str = "appcontainer";

/// The container of one sandbox; dropping it revokes its ACL entries.
#[derive(Debug)]
pub(crate) struct Prepared {
    container: AppContainer,
    launcher: PathBuf,
}

pub(crate) fn probe(policy: &Policy) -> StrengthReport {
    let net = match policy.connect() {
        Ports::All if !policy.has_host_patterns() => {
            FeatureReport::full(Feature::Net, Level::Enforced, PLATFORM)
        }
        Ports::Only(ports) if ports.is_empty() => {
            FeatureReport::full(Feature::Net, Level::Enforced, PLATFORM)
        }
        _ => FeatureReport::degraded(
            Feature::Net,
            Level::Partial,
            PLATFORM,
            Gap::new(
                DegradedFeature::Appcontainer,
                "AppContainer network capabilities cannot filter ports or hosts",
            ),
        ),
    };
    StrengthReport {
        platform: PLATFORM,
        features: vec![
            FeatureReport::full(Feature::Fs, Level::Enforced, PLATFORM),
            net,
            FeatureReport::full(Feature::Process, Level::Enforced, "job object"),
            FeatureReport::full(Feature::Syscalls, Level::Partial, PLATFORM),
        ],
    }
}

pub(crate) fn prepare(
    policy: &Policy,
    _report: &StrengthReport,
    launcher: Option<PathBuf>,
) -> Result<Prepared, Error> {
    let launcher = launcher.ok_or_else(|| setup_error("no launcher executable"))?;
    let mut container = AppContainer::create().map_err(setup_error)?;
    let grants = policy
        .read_roots()
        .iter()
        .map(|root| (root, Access::Read))
        .chain(
            policy
                .write_roots()
                .iter()
                .map(|root| (root, Access::Write)),
        );
    for (root, access) in grants {
        if root.exists() {
            container.grant(root, access).map_err(setup_error)?;
        }
    }
    for store in policy.denied() {
        if store.exists() {
            container.grant(store, Access::Deny).map_err(setup_error)?;
        }
    }
    Ok(Prepared {
        container,
        launcher,
    })
}

impl Prepared {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "one signature for every platform; Linux can fail"
    )]
    pub(crate) fn command(&self, policy: &Policy, program: &OsStr) -> Result<Command, Error> {
        let mut command = Command::new(&self.launcher);
        let internet = policy.connect().is_all();
        command.args(LaunchSpec::head(
            self.container.name(),
            internet,
            policy.may_spawn(),
            program,
        ));
        Ok(command)
    }
}
