//! Linux: Landlock (filesystem, TCP ports, signal scoping) plus a seccomp
//! filter, applied in the child between `fork` and `exec`.
//!
//! Full level on Linux is every feature `enforced`: `fs` needs Landlock,
//! `net` needs Landlock TCP rules (ABI 4, Linux 6.7) when any TCP direction
//! is restricted and seccomp when non-TCP IP sockets are, `process` and
//! `syscalls` need seccomp.

mod exec;
mod landlock;
mod seccomp;

use std::ffi::OsStr;
use std::process::Command;

use ::landlock::ABI;
use pmt_config::schema::DegradedFeature;
use pmt_core::error::Error;

use crate::policy::Policy;
use crate::report::{Feature, FeatureReport, Gap, Level, StrengthReport};
use crate::sandbox::setup_error;

const PLATFORM: &str = "landlock+seccomp";

/// What one sandbox applies to each child.
#[derive(Debug)]
pub(crate) struct Prepared {
    landlock: Option<ABI>,
    filters: Vec<seccomp::Program>,
}

pub(crate) fn probe(policy: &Policy) -> StrengthReport {
    report(policy, landlock::kernel_abi(), seccomp::supported())
}

/// The report for a kernel with Landlock `abi` (if any) and seccomp
/// support `seccomp`.
fn report(policy: &Policy, abi: Option<ABI>, seccomp: bool) -> StrengthReport {
    let no_landlock = || {
        Gap::new(
            DegradedFeature::Landlock,
            "landlock not supported by kernel",
        )
    };
    let no_seccomp = || Gap::new(DegradedFeature::Seccomp, "seccomp not supported by kernel");
    let fs = match abi {
        Some(_) => FeatureReport::full(Feature::Fs, Level::Enforced, "landlock"),
        None => FeatureReport::degraded(Feature::Fs, Level::Unavailable, "landlock", no_landlock()),
    };
    let seccomp_feature = |feature| {
        if seccomp {
            FeatureReport::full(feature, Level::Enforced, "seccomp")
        } else {
            FeatureReport::degraded(feature, Level::Unavailable, "seccomp", no_seccomp())
        }
    };
    StrengthReport {
        platform: PLATFORM,
        features: vec![
            fs,
            net_report(policy, abi, seccomp),
            seccomp_feature(Feature::Process),
            seccomp_feature(Feature::Syscalls),
        ],
    }
}

fn net_report(policy: &Policy, abi: Option<ABI>, seccomp: bool) -> FeatureReport {
    let tcp_needed = !policy.connect().is_all() || !policy.bind().is_all();
    let other_needed = !policy.connect().is_all();
    let unavailable = |hardening, reason: String| {
        FeatureReport::degraded(
            Feature::Net,
            Level::Unavailable,
            "landlock",
            Gap::new(hardening, reason),
        )
    };
    if tcp_needed && abi.is_none_or(|abi| abi < ABI::V4) {
        let reason = match abi {
            None => "landlock not supported by kernel".to_owned(),
            Some(abi) => format!(
                "landlock network rules need ABI 4 (Linux 6.7); kernel has ABI {}",
                abi as i32
            ),
        };
        return unavailable(DegradedFeature::LandlockNet, reason);
    }
    if other_needed && !seccomp {
        return unavailable(
            DegradedFeature::Seccomp,
            "seccomp not supported by kernel".to_owned(),
        );
    }
    if policy.has_host_patterns() {
        let gap = Gap::new(
            DegradedFeature::LandlockNet,
            "landlock filters ports, not hosts; host patterns in net.connect need the egress proxy",
        );
        return FeatureReport::degraded(Feature::Net, Level::Partial, "landlock", gap);
    }
    FeatureReport::full(Feature::Net, Level::Enforced, "landlock")
}

pub(crate) fn prepare(
    policy: &Policy,
    report: &StrengthReport,
    _launcher: Option<std::path::PathBuf>,
) -> Result<Prepared, Error> {
    let landlock = match report.level(Feature::Fs) {
        Level::Unavailable => None,
        _ => landlock::kernel_abi(),
    };
    let filters = match report.level(Feature::Syscalls) {
        Level::Unavailable => Vec::new(),
        _ => seccomp::programs(policy).map_err(setup_error)?,
    };
    Ok(Prepared { landlock, filters })
}

impl Prepared {
    pub(crate) fn command(&self, policy: &Policy, program: &OsStr) -> Result<Command, Error> {
        let ruleset = self
            .landlock
            .map(|abi| landlock::ruleset(policy, abi))
            .transpose()
            .map_err(setup_error)?;
        let mut command = Command::new(program);
        exec::confine(&mut command, ruleset, self.filters.clone());
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::policy::Paths;

    fn policy(caps: &[&str]) -> Policy {
        let root = Path::new("/nonexistent");
        let paths = Paths {
            user_home: root.join("u"),
            prometheus_home: root.join("u/.prometheus"),
            workspace: root.join("w"),
            scratch: vec![PathBuf::from("/nonexistent/s")],
        };
        Policy::from_capabilities(caps, &paths).unwrap()
    }

    #[test]
    fn kernel_without_landlock_names_it() {
        let report = report(&policy(crate::BASELINE), None, true);
        let gaps = report.gaps();
        assert_eq!(gaps[0].reason, "landlock not supported by kernel");
        assert_eq!(gaps[0].hardening, DegradedFeature::Landlock);
        assert_eq!(
            report.enforcement().to_string(),
            "partial:landlock,landlock_net"
        );
    }

    #[test]
    fn old_landlock_lacks_tcp_rules() {
        let report = report(&policy(crate::BASELINE), Some(ABI::V3), true);
        assert_eq!(report.level(Feature::Fs), Level::Enforced);
        assert_eq!(report.level(Feature::Net), Level::Unavailable);
        assert_eq!(report.enforcement().to_string(), "partial:landlock_net");
        let open = report_for_open_network();
        assert_eq!(
            open.enforcement().to_string(),
            "full",
            "nothing to restrict"
        );
    }

    fn report_for_open_network() -> StrengthReport {
        report(
            &policy(&["net.connect:*", "net.listen:loopback"]),
            Some(ABI::V3),
            true,
        )
    }

    #[test]
    fn full_on_a_current_kernel() {
        let report = report(&policy(crate::BASELINE), Some(ABI::V6), true);
        assert_eq!(report.enforcement().to_string(), "full");
        let hosts = super::report(
            &policy(&["net.connect:example.com:443"]),
            Some(ABI::V6),
            true,
        );
        assert_eq!(hosts.level(Feature::Net), Level::Partial);
        let no_seccomp = super::report(&policy(crate::BASELINE), Some(ABI::V6), false);
        assert_eq!(no_seccomp.enforcement().to_string(), "partial:seccomp");
    }
}
