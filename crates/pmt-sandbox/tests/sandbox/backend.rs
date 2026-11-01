//! The typed terminal backend registry, readiness checks and terminal
//! defaults (spec: execution-environments).

use pmt_config::schema::{SandboxConfig, TerminalConfig};
use pmt_sandbox::backend::{BackendRegistry, Kind};
use pmt_sandbox::{Feature, FeatureReport, Gap, Level, Options, Sandbox, StrengthReport};

use crate::common::{Fixture, launcher};

#[test]
fn unknown_backend_exits_78_with_the_available_list() {
    let err = BackendRegistry::builtin().select("acmebox").unwrap_err();
    assert_eq!(err.exit_code(), 78);
    assert_eq!(
        err.message(),
        "unknown terminal backend 'acmebox'; available: local, container, ssh"
    );

    let mut registry = BackendRegistry::builtin();
    registry.register_plugin("modal").unwrap();
    let err = registry.select("acmebox").unwrap_err();
    assert!(
        err.message()
            .ends_with("available: local, container, ssh, modal")
    );
}

#[test]
fn reserved_backends_are_available_in_m2() {
    let registry = BackendRegistry::builtin();
    for name in ["container", "ssh"] {
        let err = registry.select(name).unwrap_err();
        assert_eq!(err.exit_code(), 69);
        assert_eq!(
            err.message(),
            format!("terminal backend '{name}' is available in M2")
        );
    }
    assert_eq!(registry.select("local").unwrap().kind, Kind::Local);
}

#[test]
fn names_never_fall_back() {
    let mut registry = BackendRegistry::builtin();
    assert!(registry.select("Local").is_err());
    assert!(registry.register_plugin("local").is_err());
}

fn sandbox_with(fixture: &Fixture, report: StrengthReport) -> Sandbox {
    let options = Options {
        launcher: Some(launcher()),
        report: Some(report),
    };
    Sandbox::with_options(fixture.policy(&[]), &SandboxConfig::default(), options).unwrap()
}

fn full() -> StrengthReport {
    StrengthReport {
        platform: "test",
        features: Feature::ALL
            .iter()
            .map(|f| FeatureReport::full(*f, Level::Enforced, "test"))
            .collect(),
    }
}

#[test]
fn local_backend_is_ready_under_a_full_sandbox() {
    let fixture = Fixture::new();
    let readiness = BackendRegistry::builtin()
        .readiness(&TerminalConfig::default(), &sandbox_with(&fixture, full()));
    assert!(readiness.is_ready());
    assert_eq!(readiness.exit_code(), 0);
    assert_eq!(
        readiness.checks[0].message,
        "local backend ready; sandbox enforcement: full"
    );
}

#[test]
fn local_backend_is_not_ready_when_the_sandbox_refuses() {
    let fixture = Fixture::new();
    let mut report = full();
    report.features[0] = FeatureReport::degraded(
        Feature::Fs,
        Level::Unavailable,
        "landlock",
        Gap::new(
            pmt_config::schema::DegradedFeature::Landlock,
            "landlock not supported by kernel",
        ),
    );
    let readiness = BackendRegistry::builtin()
        .readiness(&TerminalConfig::default(), &sandbox_with(&fixture, report));
    assert_eq!(readiness.exit_code(), 1);
    assert!(
        readiness
            .reason()
            .unwrap()
            .starts_with("sandbox unavailable: landlock not supported by kernel")
    );
}

#[test]
fn ssh_misconfigured() {
    let fixture = Fixture::new();
    let mut config = TerminalConfig {
        backend: "ssh".to_owned(),
        ..TerminalConfig::default()
    };
    config.ssh.host = Some("build.example.com".to_owned());
    let readiness = BackendRegistry::builtin().readiness(&config, &sandbox_with(&fixture, full()));
    assert_eq!(
        readiness.reason(),
        Some("ssh backend requires terminal.ssh.host and terminal.ssh.user")
    );
    assert_eq!(readiness.exit_code(), 1);

    config.ssh.user = Some("ci".to_owned());
    let readiness = BackendRegistry::builtin().readiness(&config, &sandbox_with(&fixture, full()));
    assert_eq!(
        readiness.reason(),
        Some("terminal backend 'ssh' is available in M2")
    );
}

#[test]
fn unknown_backend_is_not_ready() {
    let fixture = Fixture::new();
    let config = TerminalConfig {
        backend: "acmebox".to_owned(),
        ..TerminalConfig::default()
    };
    let readiness = BackendRegistry::builtin().readiness(&config, &sandbox_with(&fixture, full()));
    assert_eq!(readiness.exit_code(), 1);
    assert!(
        readiness
            .reason()
            .unwrap()
            .starts_with("unknown terminal backend 'acmebox'")
    );
}

#[test]
fn settings_of_an_unselected_backend_do_not_block_the_selected_one() {
    let fixture = Fixture::new();
    let mut config = TerminalConfig::default();
    config.ssh.host = Some(String::new());
    config.container.cpus = 64.0;
    let readiness = BackendRegistry::builtin().readiness(&config, &sandbox_with(&fixture, full()));
    assert!(readiness.is_ready());
}

#[test]
fn terminal_defaults() {
    let config = TerminalConfig::default();
    assert_eq!(config.backend, "local");
    assert_eq!(
        (
            config.timeout_secs,
            config.max_foreground_secs,
            config.idle_lifetime_secs
        ),
        (180, 600, 300)
    );
    assert!(config.persistent_shell);
    let c = &config.container;
    assert!((c.cpus - 1.0).abs() < f64::EPSILON);
    assert_eq!(
        (c.memory_bytes, c.disk_bytes),
        (5_368_709_120, 53_687_091_200)
    );
    assert!(c.persistent);
    assert_eq!((c.shm_size.as_str(), c.pids_limit), ("1g", 2048));
    assert_eq!(config.ssh.port, 22);
}
