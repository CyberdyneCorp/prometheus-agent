//! macOS: a Seatbelt profile generated from the policy, applied by
//! `/usr/bin/sandbox-exec` before it execs the child.
//!
//! The profile denies by default and then allows: process execution, forks
//! (with `process.spawn`), signals and process inspection within the same
//! sandbox, reads of the granted roots, writes to the writable roots, the
//! standard devices, a fixed list of system services, and the network per
//! policy. Credential stores below a granted root are denied after the
//! allows (in SBPL the last matching rule wins), and a grant that names a
//! path inside a store is allowed again after the denies.
//!
//! Full level on macOS: `fs`, `net` and `process` `enforced`; `syscalls` is
//! `partial` (Seatbelt mediates operations, it does not filter raw system
//! calls) and that is the platform's maximum, so it is not a downgrade.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_config::schema::DegradedFeature;
use pmt_core::error::Error;

use crate::policy::{Policy, Ports};
use crate::report::{Feature, FeatureReport, Gap, Level, StrengthReport};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const PLATFORM: &str = "seatbelt";

/// The profile applied to every child of one sandbox.
#[derive(Debug)]
pub(crate) struct Prepared {
    profile: String,
}

pub(crate) fn probe(policy: &Policy) -> StrengthReport {
    report(policy, Path::new(SANDBOX_EXEC).exists())
}

fn report(policy: &Policy, available: bool) -> StrengthReport {
    if !available {
        let gap = Gap::new(
            DegradedFeature::Seatbelt,
            format!("seatbelt unavailable: {SANDBOX_EXEC} not found"),
        );
        return StrengthReport::none(PLATFORM, &gap);
    }
    let net = if policy.has_host_patterns() {
        let gap = Gap::new(
            DegradedFeature::Seatbelt,
            "seatbelt filters ports, not hosts; host patterns in net.connect need the egress proxy",
        );
        FeatureReport::degraded(Feature::Net, Level::Partial, PLATFORM, gap)
    } else {
        FeatureReport::full(Feature::Net, Level::Enforced, PLATFORM)
    };
    StrengthReport {
        platform: PLATFORM,
        features: vec![
            FeatureReport::full(Feature::Fs, Level::Enforced, PLATFORM),
            net,
            FeatureReport::full(Feature::Process, Level::Enforced, PLATFORM),
            FeatureReport::full(Feature::Syscalls, Level::Partial, PLATFORM),
        ],
    }
}

#[allow(clippy::unnecessary_wraps, reason = "one signature for every platform")]
pub(crate) fn prepare(
    policy: &Policy,
    _report: &StrengthReport,
    _launcher: Option<PathBuf>,
) -> Result<Prepared, Error> {
    Ok(Prepared {
        profile: profile(policy),
    })
}

impl Prepared {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "one signature for every platform; Linux can fail"
    )]
    pub(crate) fn command(&self, _policy: &Policy, program: &OsStr) -> Result<Command, Error> {
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(&self.profile).arg("--").arg(program);
        Ok(command)
    }
}

/// System services common tools reach over Mach IPC (user and group
/// lookups, notifications, logging, TLS trust evaluation, DNS, preferences).
/// The keychain service (`com.apple.SecurityServer`) is deliberately absent.
const MACH_SERVICES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.system.DirectoryService.libinfo_v1",
    "com.apple.system.notification_center",
    "com.apple.system.logger",
    "com.apple.logd",
    "com.apple.diagnosticd",
    "com.apple.trustd",
    "com.apple.trustd.agent",
    "com.apple.dnssd.service",
    "com.apple.SystemConfiguration.configd",
    "com.apple.SystemConfiguration.DNSConfiguration",
    "com.apple.cfprefsd.daemon",
    "com.apple.cfprefsd.agent",
    "com.apple.coreservices.launchservicesd",
    "com.apple.lsd.mapdb",
];

const BASE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow user-preference-read)
(allow ipc-posix-shm-read-data)
(allow pseudo-tty)
(allow file-ioctl)
(allow file-read* (literal "/"))
(allow file-read* file-write* (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/ttys[0-9]+$"))
(allow file-read* (literal "/dev/random") (literal "/dev/urandom"))
"#;

/// The Seatbelt profile for `policy`.
pub(crate) fn profile(policy: &Policy) -> String {
    let mut sbpl = String::from(BASE);
    if policy.may_spawn() {
        sbpl.push_str("(allow process-fork)\n");
    }
    for service in MACH_SERVICES {
        let _ = writeln!(sbpl, "(allow mach-lookup (global-name {}))", quote(service));
    }
    filesystem(policy, &mut sbpl);
    network(policy, &mut sbpl);
    sbpl
}

fn filesystem(policy: &Policy, sbpl: &mut String) {
    for root in policy.read_roots() {
        let _ = writeln!(sbpl, "(allow file-read* (subpath {}))", path(root));
    }
    for root in policy.write_roots() {
        let _ = writeln!(
            sbpl,
            "(allow file-read* file-write* (subpath {}))",
            path(root)
        );
    }
    let denied = policy.denied();
    for store in &denied {
        let _ = writeln!(
            sbpl,
            "(deny file-read* file-write* (subpath {}))",
            path(store)
        );
    }
    let regranted = |root: &&PathBuf| denied.iter().any(|store| root.starts_with(store));
    for root in policy.read_roots().iter().filter(regranted) {
        let _ = writeln!(sbpl, "(allow file-read* (subpath {}))", path(root));
    }
    for root in policy.write_roots().iter().filter(regranted) {
        let _ = writeln!(
            sbpl,
            "(allow file-read* file-write* (subpath {}))",
            path(root)
        );
    }
}

fn network(policy: &Policy, sbpl: &mut String) {
    let connect = !matches!(policy.connect(), Ports::Only(ports) if ports.is_empty());
    if connect {
        // DNS goes through mDNSResponder's socket.
        sbpl.push_str("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n(allow system-socket)\n");
    }
    match policy.connect() {
        Ports::All => sbpl.push_str("(allow network-outbound (remote ip))\n"),
        Ports::Only(ports) => {
            for port in ports {
                let _ = writeln!(sbpl, "(allow network-outbound (remote tcp \"*:{port}\"))");
            }
        }
    }
    match policy.bind() {
        Ports::All => sbpl.push_str(
            "(allow network-bind (local ip \"localhost:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n",
        ),
        Ports::Only(ports) => {
            for port in ports {
                let _ = writeln!(
                    sbpl,
                    "(allow network-bind (local tcp \"localhost:{port}\"))\n(allow network-inbound (local tcp \"localhost:{port}\"))"
                );
            }
        }
    }
}

fn path(path: &Path) -> String {
    quote(&path.to_string_lossy())
}

/// An SBPL string literal.
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Paths;

    #[test]
    fn report_levels_for_seatbelt() {
        let paths = Paths {
            user_home: PathBuf::from("/nonexistent/u"),
            prometheus_home: PathBuf::from("/nonexistent/u/.prometheus"),
            workspace: PathBuf::from("/nonexistent/w"),
            scratch: Vec::new(),
        };
        let policy = Policy::baseline(&paths).unwrap();
        let full = report(&policy, true);
        let levels: Vec<(&str, &str)> = full
            .features
            .iter()
            .map(|f| (f.feature.as_str(), f.level.as_str()))
            .collect();
        assert_eq!(
            levels,
            [
                ("fs", "enforced"),
                ("net", "enforced"),
                ("process", "enforced"),
                ("syscalls", "partial")
            ]
        );
        assert_eq!(full.enforcement().to_string(), "full");
        assert_eq!(report(&policy, false).enforcement().to_string(), "none");
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
