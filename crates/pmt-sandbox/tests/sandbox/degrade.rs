//! Hardening downgrades are never silent (spec: security-boundaries;
//! execution-environments, Local backend runs under the OS sandbox): a
//! missing feature refuses to run commands unless
//! `security.sandbox.allow_degraded` names it, and an allowed one is logged
//! at WARN on every start.

use pmt_config::schema::{DegradedFeature, SandboxConfig, SandboxMode};
use pmt_sandbox::{Feature, FeatureReport, Gap, Level, Options, Sandbox, StrengthReport};

use crate::common::{Fixture, capture_warnings, confine, launcher};

/// A Linux kernel without Landlock.
fn no_landlock() -> StrengthReport {
    let gap = |hardening| Gap::new(hardening, "landlock not supported by kernel");
    StrengthReport {
        platform: "landlock+seccomp",
        features: vec![
            FeatureReport::degraded(
                Feature::Fs,
                Level::Unavailable,
                "landlock",
                gap(DegradedFeature::Landlock),
            ),
            FeatureReport::degraded(
                Feature::Net,
                Level::Unavailable,
                "landlock",
                gap(DegradedFeature::LandlockNet),
            ),
            FeatureReport::full(Feature::Process, Level::Enforced, "seccomp"),
            FeatureReport::full(Feature::Syscalls, Level::Enforced, "seccomp"),
        ],
    }
}

fn sandbox(fixture: &Fixture, config: &SandboxConfig, report: Option<StrengthReport>) -> Sandbox {
    let options = Options {
        launcher: Some(launcher()),
        report,
    };
    Sandbox::with_options(fixture.policy(&[]), config, options).unwrap()
}

#[test]
fn unavailable_feature_without_allow_degraded_refuses_to_run_commands() {
    let fixture = Fixture::new();
    let sandbox = sandbox(&fixture, &SandboxConfig::default(), Some(no_landlock()));
    let err = sandbox.command("ls").unwrap_err();
    assert_eq!(
        err.message(),
        "sandbox unavailable: landlock not supported by kernel; set security.sandbox.allow_degraded = [\"landlock\"] to run without it"
    );
    assert_eq!(err.exit_code(), 77);
    assert_eq!(
        sandbox.report().enforcement().to_string(),
        "partial:landlock,landlock_net"
    );
}

#[test]
fn naming_only_some_missing_features_still_refuses() {
    let fixture = Fixture::new();
    let config = SandboxConfig {
        allow_degraded: vec![DegradedFeature::Landlock],
        ..SandboxConfig::default()
    };
    let err = sandbox(&fixture, &config, Some(no_landlock()))
        .command("ls")
        .unwrap_err();
    assert!(
        err.message().contains("[\"landlock_net\"]"),
        "{}",
        err.message()
    );
}

#[test]
fn allowed_downgrade_runs_and_warns_on_every_start() {
    let fixture = Fixture::new();
    let config = SandboxConfig {
        allow_degraded: vec![DegradedFeature::Landlock, DegradedFeature::LandlockNet],
        ..SandboxConfig::default()
    };
    for _start in 0..2 {
        let (sandbox, warnings) =
            capture_warnings(|| sandbox(&fixture, &config, Some(no_landlock())));
        assert!(sandbox.command("ls").is_ok());
        assert!(warnings.contains("WARN"), "{warnings}");
        assert!(warnings.contains("feature=\"landlock\""), "{warnings}");
        assert!(warnings.contains("feature=\"landlock_net\""), "{warnings}");
        assert_eq!(
            sandbox.banner_marker().as_deref(),
            Some("sandbox: partial:landlock,landlock_net")
        );
        assert_eq!(
            sandbox.downgrades().len(),
            2,
            "one security.downgrade event each"
        );
    }
}

#[test]
fn full_enforcement_is_quiet() {
    let fixture = Fixture::new();
    let full = StrengthReport {
        platform: "test",
        features: pmt_sandbox::Feature::ALL
            .iter()
            .map(|f| FeatureReport::full(*f, Level::Enforced, "test"))
            .collect(),
    };
    let (sandbox, warnings) =
        capture_warnings(|| sandbox(&fixture, &SandboxConfig::default(), Some(full)));
    assert!(warnings.is_empty(), "{warnings}");
    assert!(sandbox.banner_marker().is_none());
    assert!(sandbox.check().is_ok());
}

#[test]
fn mode_off_needs_all() {
    let fixture = Fixture::new();
    let off = SandboxConfig {
        mode: SandboxMode::Off,
        allow_degraded: vec![DegradedFeature::Landlock],
    };
    let err = sandbox(&fixture, &off, None).command("ls").unwrap_err();
    assert!(
        err.message()
            .starts_with("sandbox unavailable: security.sandbox.mode is off"),
        "{}",
        err.message()
    );

    let allowed = SandboxConfig {
        mode: SandboxMode::Off,
        allow_degraded: vec![DegradedFeature::All],
    };
    let (sandbox, warnings) = capture_warnings(|| sandbox(&fixture, &allowed, None));
    assert_eq!(sandbox.report().enforcement().to_string(), "none");
    assert!(warnings.contains("feature=\"all\""), "{warnings}");
    let output = sandbox
        .command(if cfg!(windows) { "cmd.exe" } else { "true" })
        .unwrap();
    assert_eq!(
        output.get_program(),
        if cfg!(windows) { "cmd.exe" } else { "true" },
        "runs unconfined"
    );
}

#[test]
fn host_report_lists_every_feature() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let features: Vec<&str> = sandbox
        .report()
        .features
        .iter()
        .map(|f| f.feature.as_str())
        .collect();
    assert_eq!(features, ["fs", "net", "process", "syscalls"]);
    let checks = sandbox.report().doctor_checks();
    assert_eq!(checks.len(), 4);
    assert!(checks.iter().all(|c| c.group == "security"));
}
