//! The Seatbelt backend's report (spec: security-boundaries, Doctor report).

use pmt_sandbox::{Feature, Level};

use crate::common::{Fixture, confine};

#[test]
fn doctor_report_lists_seatbelt_levels() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "seatbelt");
    let levels: Vec<(Feature, Level)> = report
        .features
        .iter()
        .map(|f| (f.feature, f.level))
        .collect();
    assert_eq!(
        levels,
        [
            (Feature::Fs, Level::Enforced),
            (Feature::Net, Level::Enforced),
            (Feature::Process, Level::Enforced),
            (Feature::Syscalls, Level::Partial),
        ]
    );
    let lines: Vec<String> = report
        .doctor_checks()
        .into_iter()
        .map(|c| c.message)
        .collect();
    assert_eq!(lines[0], "fs: enforced (seatbelt)");
    assert_eq!(lines[3], "syscalls: partial (seatbelt)");
}
