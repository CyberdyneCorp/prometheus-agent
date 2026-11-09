//! The built-in `singularity` backend (spec: execution-environments,
//! Singularity backend; Child environments are built from an allowlist).
//!
//! The always-run tests drive a fake `apptainer` CLI (a shell script whose
//! instances are directories and whose `exec` runs the command with the
//! local `sh`), so the start arguments, the instance sharing and the way
//! variables travel are checked on every host. `real_apptainer_runs_the_image`
//! uses the installed Apptainer: it skips without one unless
//! `PMT_TEST_APPTAINER=require`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::TerminalConfig;
use pmt_core::secret::SecretString;
use pmt_sandbox::backend::Descriptor;
use pmt_sandbox::singularity::{Instance, InstanceSpec, Runtime};
use pmt_tools::terminal::remote::singularity::SingularityTransport;
use pmt_tools::terminal::remote::{CONTAINER_STATE, RemoteTarget, Transport};
use pmt_tools::terminal::{Status, Terminal, TerminalArgs};

use crate::common::{Fixture, KNOWN_SECRET};

/// Instances are directories under `$FAKE_ROOT/inst`; every call is
/// appended to `$FAKE_ROOT/calls.log`. `exec` exports each
/// `APPTAINERENV_<NAME>` as `<NAME>`, sets `HOME` to the instance's home
/// and runs the command there.
const FAKE_APPTAINER: &str = r#"#!/bin/sh
root=$FAKE_ROOT
unset FAKE_ROOT
printf '%s\n' "$*" >> "$root/calls.log"
case "$1 $2" in
"instance start")
    for a; do name=$a; done
    if [ -d "$root/inst/$name" ]; then
        echo "FATAL: instance $name already exists" >&2
        exit 255
    fi
    mkdir -p "$root/inst/$name/home"
    exit 0 ;;
"instance stop")
    rm -rf "$root/inst/$3"
    exit 0 ;;
esac
[ "$1" = exec ] || exit 2
shift
[ "$1" = --cleanenv ] || { echo "exec without --cleanenv" >&2; exit 2; }
shift
name=${1#instance://}
shift
[ -d "$root/inst/$name" ] || { echo "FATAL: no instance $name" >&2; exit 255; }
for n in $(env | sed -n 's/^APPTAINERENV_\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p'); do
    eval "export $n=\"\$APPTAINERENV_$n\""
    unset "APPTAINERENV_$n"
done
export HOME="$root/inst/$name/home"
cd "$HOME" || exit 255
exec "$@"
"#;

struct Fake {
    root: tempfile::TempDir,
    runtime: Runtime,
}

impl Fake {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let program = root.path().join("apptainer");
        std::fs::write(&program, FAKE_APPTAINER).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (
                OsString::from("FAKE_ROOT"),
                root.path().as_os_str().to_owned(),
            ),
        ];
        Self {
            runtime: Runtime::at(program, env),
            root,
        }
    }

    fn spec(&self) -> InstanceSpec {
        InstanceSpec {
            profile: "default".to_owned(),
            image: "docker://alpine:3.20".to_owned(),
            overlay: self
                .root
                .path()
                .join("profile/terminal/singularity-overlay"),
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.path().join("calls.log")).unwrap_or_default()
    }
}

fn session(
    fixture: &Fixture,
    transport: Arc<dyn Transport>,
    secrets: Vec<(String, SecretString)>,
) -> Terminal {
    let target = Arc::new(RemoteTarget::new(
        transport,
        Descriptor::singularity(),
        "ses_singularity",
        CONTAINER_STATE,
    ));
    let env = fixture.child_env(&[], &[]).with_secrets(secrets);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.target = target;
    setup.start_cwd = PathBuf::from("~");
    Terminal::new(setup).unwrap()
}

fn transport(runtime: &Runtime, spec: &InstanceSpec) -> Arc<dyn Transport> {
    Arc::new(SingularityTransport::new(Instance::shared(
        runtime.clone(),
        spec.clone(),
    )))
}

#[tokio::test(flavor = "multi_thread")]
async fn singularity_commands_run_in_the_profile_instance() {
    let fake = Fake::new();
    let spec = fake.spec();
    let fixture = Fixture::new();
    let first = session(
        &fixture,
        transport(&fake.runtime, &spec),
        vec![("PMT_TEST_TOKEN".to_owned(), SecretString::new(KNOWN_SECRET))],
    );
    let home = fake.root.path().join("inst").join(spec.name()).join("home");
    let out = first
        .run(TerminalArgs::new(
            "echo \"$HOME|$(pwd)|len=${#PMT_TEST_TOKEN}\"",
        ))
        .await;
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    let home = home.canonicalize().unwrap();
    assert_eq!(
        out.output.trim(),
        format!("{h}|{h}|len={}", KNOWN_SECRET.len(), h = home.display()),
        "{out:?}"
    );
    // Started once, contained, on the profile's overlay (created).
    let calls = fake.calls();
    let start = format!(
        "instance start --containall --overlay {} docker://alpine:3.20 {}",
        spec.overlay.display(),
        spec.name()
    );
    assert_eq!(calls.matches(&start).count(), 1, "{calls}");
    assert!(spec.overlay.is_dir());
    // The secret travelled on the client's environment, never its argv,
    // and the host's own locations stay behind.
    assert!(!calls.contains(KNOWN_SECRET), "{calls}");
    let env = first.run(TerminalArgs::new("env")).await;
    assert!(
        !env.output
            .contains(&fixture.home.to_string_lossy().into_owned())
    );

    // A second session of the profile shares the running instance.
    let second = session(&fixture, transport(&fake.runtime, &spec), Vec::new());
    let out = second.run(TerminalArgs::new("echo second")).await;
    assert_eq!(out.output.trim(), "second", "{out:?}");
    assert_eq!(fake.calls().matches(&start).count(), 1);

    // The instance stops when the last session ends; the overlay stays.
    first.close().await;
    assert!(!fake.calls().contains("instance stop"));
    second.close().await;
    assert_eq!(
        fake.calls()
            .matches(&format!("instance stop {}", spec.name()))
            .count(),
        1
    );
    assert!(spec.overlay.is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn singularity_instance_failure_is_reported() {
    let fake = Fake::new();
    let mut spec = fake.spec();
    // An overlay that cannot be created fails the start with the reason.
    let blocker = fake.root.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    spec.overlay = blocker.join("overlay");
    let fixture = Fixture::new();
    let terminal = session(&fixture, transport(&fake.runtime, &spec), Vec::new());
    let out = terminal.run(TerminalArgs::new("true")).await;
    assert_eq!(out.status, Status::Degraded, "{out:?}");
    assert!(
        out.reason
            .as_deref()
            .is_some_and(|r| r.contains("cannot create the overlay")),
        "{out:?}"
    );
    terminal.close().await;
}

/// The installed Apptainer, or `None` (skip) unless required.
fn real_runtime() -> Option<Runtime> {
    match Runtime::detect() {
        Ok(runtime) => Some(runtime),
        Err(err) if std::env::var("PMT_TEST_APPTAINER").as_deref() == Ok("require") => {
            panic!("PMT_TEST_APPTAINER=require but {}", err.message())
        }
        Err(err) => {
            eprintln!("skipping apptainer test: {}", err.message());
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_apptainer_runs_the_image() {
    let Some(runtime) = real_runtime() else {
        return;
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = InstanceSpec {
        profile: "pmt-test".to_owned(),
        image: std::env::var("PMT_TEST_APPTAINER_IMAGE")
            .unwrap_or_else(|_| "docker://alpine:3.20".to_owned()),
        overlay: dir.path().join("overlay"),
    };
    let fixture = Fixture::new();
    let terminal = session(
        &fixture,
        transport(&runtime, &spec),
        vec![("PMT_TEST_TOKEN".to_owned(), SecretString::new(KNOWN_SECRET))],
    );
    let out = terminal
        .run(TerminalArgs::new(
            "cat /etc/alpine-release >/dev/null && echo \"image ok len=${#PMT_TEST_TOKEN}\"",
        ))
        .await;
    assert_eq!(
        out.output.trim(),
        format!("image ok len={}", KNOWN_SECRET.len()),
        "{out:?}"
    );
    // Contained: the host's home is not visible.
    let host_home = fixture.home.to_string_lossy().into_owned();
    let ls = terminal
        .run(TerminalArgs::new(format!(
            "ls {host_home} 2>&1 || echo absent"
        )))
        .await;
    assert!(
        ls.output.contains("absent") || ls.output.trim().is_empty(),
        "{ls:?}"
    );
    terminal.close().await;
    assert!(Path::new(&spec.overlay).is_dir());
}
