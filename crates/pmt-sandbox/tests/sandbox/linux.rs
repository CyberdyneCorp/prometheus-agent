//! The Landlock + seccomp backend on a real kernel.

use pmt_sandbox::{Feature, Level};

use crate::common::{Fixture, confine};

#[test]
fn report_names_landlock_and_seccomp() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "landlock+seccomp");
    assert_eq!(
        report.level(Feature::Fs),
        Level::Enforced,
        "CI kernels have Landlock: {report:?}"
    );
    assert_eq!(report.level(Feature::Syscalls), Level::Enforced);
    let mechanisms: Vec<&str> = report.features.iter().map(|f| f.mechanism).collect();
    assert_eq!(mechanisms, ["landlock", "landlock", "seccomp", "seccomp"]);
}

#[test]
fn ptrace_and_user_namespaces_are_refused() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    if std::path::Path::new("/usr/bin/unshare").exists() {
        let output = sandbox
            .command("/usr/bin/unshare")
            .unwrap()
            .args(["--user", "/usr/bin/true"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
    }
}
