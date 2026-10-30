//! Child environment, working directory and pre-execution guards (spec:
//! execution-environments, Child environments are built from an allowlist;
//! Non-secret environment passthrough; Session cwd tracking; Terminal
//! argument validation; Pre-execution guards fail closed; Local backend
//! runs under the OS sandbox).

use std::sync::Arc;

use pmt_config::schema::{DegradedFeature, TerminalConfig};
use pmt_core::secret::SecretString;
use pmt_sandbox::{Feature, FeatureReport, Gap, Level, StrengthReport};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest, granted_secrets};
use pmt_tools::terminal::guard::{Guard, GuardChain, GuardFuture, GuardRequest};
use pmt_tools::terminal::{LocalTerminal, Status, TerminalArgs};

use crate::common::{BOT_TOKEN, Fixture, run};

#[tokio::test]
async fn the_parents_secrets_never_reach_a_shell() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let result = run(&session, "env").await;
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let env = result.output;
    for leaked in [
        "TELEGRAM_BOT_TOKEN",
        BOT_TOKEN,
        "OPENAI_API_KEY",
        "sk-parent-openai-key",
        "PMT_SECRET_GITHUB",
        "ghp_parent_secret_value",
        // Set by cargo for this test process: the child env is not a copy.
        "CARGO_MANIFEST_DIR",
    ] {
        assert!(!env.contains(leaked), "{leaked} reached the child:\n{env}");
    }
    assert!(env.contains("PATH=") && env.contains("LANG=C"), "{env}");
    let tmpdir = format!("TMPDIR={}", fixture.scratch.display());
    assert!(env.contains(&tmpdir), "{env}");
}

#[tokio::test]
async fn a_declared_and_granted_secret_is_the_only_one_present() {
    let fixture = Fixture::new();
    let declared = ["OPENAI_API_KEY".to_owned()];
    let secrets = granted_secrets(
        &declared,
        |name| name == "OPENAI_API_KEY",
        |_| Ok(SecretString::new("sk-granted-value-123456")),
    )
    .unwrap();
    let env = ChildEnv::build(
        &fixture.launch(&[]),
        EnvRequest {
            secrets,
            ..EnvRequest::default()
        },
    );
    let setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    let session = LocalTerminal::new(setup).unwrap();
    let result = run(&session, "env").await;
    assert!(
        result.output.contains("OPENAI_API_KEY="),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("TELEGRAM_BOT_TOKEN")
            && !result.output.contains("PMT_SECRET_GITHUB")
    );
    // The granted secret works but stays out of the persisted shell state.
    let snapshot = std::fs::read_to_string(fixture.scratch.join("pmt-shell-snapshot.sh")).unwrap();
    assert!(!snapshot.contains("sk-granted-value"), "{snapshot}");
}

#[tokio::test]
async fn a_passthrough_variable_carries_its_launch_value() {
    let fixture = Fixture::new();
    let terminal = TerminalConfig {
        env_passthrough: vec!["JAVA_HOME".to_owned()],
        ..TerminalConfig::default()
    };
    let env = fixture.child_env(&terminal.env_passthrough, &[("JAVA_HOME", "/opt/jdk-21")]);
    let session =
        LocalTerminal::new(fixture.setup(terminal, env, fixture.sandbox(&[], None))).unwrap();
    let result = run(&session, "echo $JAVA_HOME").await;
    assert_eq!(result.output, "/opt/jdk-21\n");
}

#[tokio::test]
async fn workdir_does_not_move_the_session_but_cd_does() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.work.join("sub")).unwrap();
    let session = fixture.session();
    let elsewhere = TerminalArgs {
        workdir: Some("sub".to_owned()),
        ..TerminalArgs::new("pwd")
    };
    let result = session.run(elsewhere).await;
    assert_eq!(
        result.output.trim(),
        fixture.work.join("sub").to_str().unwrap()
    );
    assert_eq!(result.cwd, None, "the command ended where it started");
    assert_eq!(
        run(&session, "pwd").await.output.trim(),
        fixture.work.to_str().unwrap()
    );

    let moved = run(&session, "cd sub").await;
    assert_eq!(moved.cwd.as_deref(), fixture.work.join("sub").to_str());
    assert_eq!(
        run(&session, "pwd").await.output.trim(),
        fixture.work.join("sub").to_str().unwrap()
    );
    assert_eq!(session.cwd(), fixture.work.join("sub"));
}

#[tokio::test]
async fn invalid_arguments_run_nothing() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let marker = fixture.work.join("ran");
    let pty = TerminalArgs {
        pty: true,
        ..TerminalArgs::new(format!("touch {}", marker.display()))
    };
    let result = session.run(pty).await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result
            .error
            .unwrap()
            .starts_with("pty requires background=true")
    );
    let zero = TerminalArgs {
        timeout: Some(0.0),
        ..TerminalArgs::new(format!("touch {}", marker.display()))
    };
    let result = session.run(zero).await;
    assert_eq!(
        result.error.unwrap(),
        "timeout must be a positive number of seconds (got 0)"
    );
    assert!(!marker.exists());
}

/// An approval transport that never answers.
struct Wedged;

impl Guard for Wedged {
    fn name(&self) -> &'static str {
        "approval"
    }

    fn check<'a>(&'a self, _: &'a GuardRequest) -> GuardFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn a_wedged_guard_fails_closed() {
    let fixture = Fixture::new();
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.guards = GuardChain::new(vec![Arc::new(Wedged)]);
    let session = LocalTerminal::new(setup).unwrap();
    let marker = fixture.work.join("ran");
    let args = TerminalArgs {
        timeout: Some(5.0),
        ..TerminalArgs::new(format!("touch {}", marker.display()))
    };
    let result = session.run(args).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.error.unwrap(),
        "pre-execution guard did not finish within 30s; the command was not run"
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn a_missing_workdir_is_refused_before_running() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let args = TerminalArgs {
        workdir: Some("missing".to_owned()),
        ..TerminalArgs::new("true")
    };
    let result = session.run(args).await;
    assert!(result.error.unwrap().starts_with("workdir does not exist"));
}

#[tokio::test]
async fn kernel_without_landlock_refuses_to_run() {
    let fixture = Fixture::new();
    let gap = |hardening| Gap::new(hardening, "landlock not supported by kernel");
    let report = StrengthReport {
        platform: "landlock+seccomp",
        features: vec![
            FeatureReport::degraded(
                Feature::Fs,
                Level::Unavailable,
                "landlock",
                gap(DegradedFeature::Landlock),
            ),
            FeatureReport::full(Feature::Net, Level::Enforced, "seccomp"),
            FeatureReport::full(Feature::Process, Level::Enforced, "seccomp"),
            FeatureReport::full(Feature::Syscalls, Level::Enforced, "seccomp"),
        ],
    };
    let env = fixture.child_env(&[], &[]);
    let setup = fixture.setup(
        TerminalConfig::default(),
        env,
        fixture.sandbox(&[], Some(report)),
    );
    let session = LocalTerminal::new(setup).unwrap();
    let marker = fixture.work.join("ran");
    let result = run(&session, &format!("touch {}", marker.display())).await;
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .contains("sandbox unavailable: landlock not supported by kernel"),
        "{result:?}"
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn writing_outside_the_workspace_fails_under_the_sandbox() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let target = fixture.outside.join("x");
    let result = run(&session, &format!("touch {}", target.display())).await;
    assert_eq!(result.status, Status::Error, "{result:?}");
    assert!(!target.exists());
    let inside = run(&session, "touch inside && echo ok").await;
    assert_eq!(inside.output, "ok\n", "{inside:?}");
}
