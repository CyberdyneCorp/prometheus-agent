//! Windows: an `AppContainer` (filesystem through ACL entries for its SID,
//! network through capabilities) inside a Job Object, started by the
//! launcher process ([`crate::launcher`]). Credential stores are carved out
//! of granted roots ([`crate::carve`]) rather than denied, so the container
//! only ever holds allow entries.
//!
//! Full level on Windows: `fs`, `net` and `process` `enforced`; `syscalls`
//! is `partial` (the `AppContainer` token gates object access, there is no
//! system-call filter) and that is the platform's maximum. `net` is
//! `enforced` when the policy allows all public connections
//! (`internetClient`) or none; capabilities cannot filter ports or hosts, so
//! a port or host list is `partial`. Listening is never granted (loopback
//! needs a per-container exemption), which is narrower than any grant.

mod acl;
mod args;
pub(crate) mod console;
mod container;
pub(crate) mod launch;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_config::schema::DegradedFeature;
use pmt_core::error::Error;

pub(crate) use args::LaunchSpec;
use container::{Access, AppContainer};

use crate::carve::{Reach, carve};
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
    let denied = policy.denied();
    let roots = policy
        .read_roots()
        .iter()
        .map(|root| (root, Access::Read))
        .chain(
            policy
                .write_roots()
                .iter()
                .map(|root| (root, Access::Write)),
        );
    for (root, access) in roots {
        for (path, reach) in carve(root, &denied) {
            let access = if reach == Reach::Subtree {
                access
            } else {
                Access::List
            };
            if path.exists() {
                container.grant(&path, access).map_err(setup_error)?;
            }
        }
    }
    open_ancestors(&mut container, policy);
    Ok(Prepared {
        container,
        launcher,
    })
}

/// Lets the container list and stat the directories above its roots.
/// PowerShell resolves a location component by component (attributes of
/// each directory, then a listing of the parent to correct the leaf's
/// case), so without this `Set-Location` into the workspace is denied and
/// the shell starts at `C:\`. Only names and attributes are exposed (no
/// file contents), as `file-read-metadata` is on macOS. Volume roots are
/// left alone, and a directory whose DACL this user may not change (for
/// example `C:\Users` without elevation) is skipped with a WARN: confined
/// PowerShell cannot enter locations below it.
fn open_ancestors(container: &mut AppContainer, policy: &Policy) {
    let roots: Vec<&PathBuf> = policy
        .read_roots()
        .iter()
        .chain(policy.write_roots())
        .collect();
    // A directory inside a root already has its grant (rewriting it here
    // would replace that entry).
    let ancestors: BTreeSet<&Path> = roots
        .iter()
        .flat_map(|root| root.ancestors().skip(1))
        .filter(|dir| !roots.iter().any(|root| dir.starts_with(root)))
        .filter(|dir| dir.parent().is_some() && dir.is_dir())
        .collect();
    for dir in ancestors {
        if let Err(why) = container.grant_listing(dir) {
            tracing::warn!(
                directory = %dir.display(),
                "sandboxed PowerShell cannot enter locations below this directory: {why}"
            );
        }
    }
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
