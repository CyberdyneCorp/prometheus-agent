//! The `container` backend against a real rootless runtime (spec:
//! execution-environments, Container hardening is never silently dropped;
//! Container lifecycle and orphan reaping; Container backend options;
//! Backend isolation descriptor; Infrastructure failures are degraded
//! results).
//!
//! These tests need Podman or Docker (`PMT_TEST_CONTAINER_RUNTIME`, default
//! `auto`). Where none is reachable they skip with a notice, unless
//! `PMT_TEST_CONTAINERS=require` (set by the Linux CI job), which turns a
//! missing runtime into a failure. A rootful daemon is accepted here (CI's
//! Docker is one): the rootless check has its own tests in pmt-sandbox, and
//! every hardening flag is still required.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{ContainerConfig, DegradedFeature, SandboxConfig, TerminalConfig};
use pmt_sandbox::backend::Descriptor;
use pmt_sandbox::container::{ContainerSpec, Pool, Runtime};
use pmt_tools::files::{FileBackend, RemoteFiles};
use pmt_tools::terminal::remote::container::ContainerTransport;
use pmt_tools::terminal::remote::{CONTAINER_STATE, RemoteExec, RemoteTarget, Transport};
use pmt_tools::terminal::{Status, Terminal, TerminalArgs};

use crate::common::Fixture;

/// A small image every runtime can pull, pinned by digest.
pub(crate) const IMAGE: &str = "docker.io/library/alpine:3@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a";

/// The runtime, or `None` (with a notice) when the tests must skip.
pub(crate) fn runtime() -> Option<Runtime> {
    let setting = std::env::var("PMT_TEST_CONTAINER_RUNTIME").unwrap_or_else(|_| "auto".to_owned());
    let rootful_ok = SandboxConfig {
        allow_degraded: vec![DegradedFeature::All],
        ..SandboxConfig::default()
    };
    let found =
        Runtime::detect(&setting).and_then(|runtime| runtime.probe(&rootful_ok).map(|_| runtime));
    match found {
        Ok(runtime) => Some(runtime),
        Err(err) if std::env::var("PMT_TEST_CONTAINERS").as_deref() == Ok("require") => {
            panic!(
                "PMT_TEST_CONTAINERS=require but no rootless runtime: {}",
                err.message()
            )
        }
        Err(err) => {
            eprintln!("skipping container test: {}", err.message());
            None
        }
    }
}

/// A session on its own container.
pub(crate) struct Session {
    pub(crate) terminal: Arc<Terminal>,
    pub(crate) transport: Arc<ContainerTransport>,
    pub(crate) files: RemoteFiles,
    pub(crate) fixture: Fixture,
}

impl Session {
    /// A session on a container of its own (never persistent).
    pub(crate) fn new(runtime: Runtime, name: &str, config: ContainerConfig) -> Self {
        let config = ContainerConfig {
            persistent: false,
            ..config
        };
        Self::open(runtime, name, config, "pmt-test")
    }

    /// A session of `profile` on the container `config` resolves to.
    pub(crate) fn open(
        runtime: Runtime,
        name: &str,
        config: ContainerConfig,
        profile: &str,
    ) -> Self {
        let fixture = Fixture::new();
        let session = format!(
            "{name}-{}",
            ulid::Ulid::generate().to_string().to_lowercase()
        );
        let spec = ContainerSpec {
            runtime,
            profile: profile.to_owned(),
            session: session.clone(),
            image: IMAGE.to_owned(),
            allow_degraded: Vec::new(),
            workspace: config.mount_workspace.then(|| fixture.work.clone()),
            volumes: Vec::new(),
            user: None,
            idle_lifetime: Duration::from_mins(5),
            config,
        };
        let mount = spec.config.mount_workspace;
        let persistent = spec.config.persistent;
        let transport = Arc::new(ContainerTransport::new(Pool::process().container(spec)));
        let target = Arc::new(RemoteTarget::new(
            Arc::clone(&transport) as Arc<dyn Transport>,
            Descriptor::container(mount, persistent),
            &session,
            CONTAINER_STATE,
        ));
        let env = fixture.child_env(&[], &[]);
        let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
        setup.target = target;
        setup.start_cwd = if mount { "/workspace" } else { "/" }.into();
        let terminal = Arc::new(Terminal::new(setup).unwrap());
        let activity: Arc<dyn pmt_sandbox::container::Activity> = Arc::clone(&terminal) as _;
        transport.attach(Arc::downgrade(&activity));
        let exec: Arc<dyn RemoteExec> = Arc::new(Arc::clone(&transport) as Arc<dyn Transport>);
        Self {
            terminal,
            files: RemoteFiles::new("container", exec),
            transport,
            fixture,
        }
    }

    pub(crate) async fn run(&self, command: &str) -> pmt_tools::terminal::TerminalResult {
        self.terminal.run(TerminalArgs::new(command)).await
    }

    pub(crate) async fn close(&self) {
        self.terminal.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn container_backend() {
    let Some(runtime) = runtime() else { return };
    let config = ContainerConfig {
        mount_workspace: false,
        ..ContainerConfig::default()
    };
    let session = Session::new(runtime, "hostname", config);
    let hostname = session.run("hostname && pwd").await;
    assert_eq!(hostname.exit_code, Some(0), "{hostname:?}");
    let mut lines = hostname.output.lines();
    let (hostname, cwd) = (lines.next().unwrap_or_default(), lines.next());
    // No workspace is mounted, so the shell starts at the container's root.
    assert_eq!(cwd, Some("/"), "{hostname}");
    let mounts = session.run("mount").await;
    assert!(!mounts.output.contains("/workspace"), "{}", mounts.output);
    // The file tools read the container's own files, never the host's.
    let read = session.files.read(Path::new("/etc/hostname")).unwrap();
    let read = String::from_utf8(read).unwrap();
    assert_eq!(read.trim(), hostname.trim());
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    assert_ne!(read.trim(), host.trim());
    assert!(!session.files.uses_host_filesystem());
    session.close().await;
}

/// Removes a container when dropped, so a failed assertion leaves nothing
/// behind.
struct Removed<'a>(&'a Runtime, String);

impl Drop for Removed<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["rm".into(), "--force".into(), self.1.clone()]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_container_is_reused_by_a_later_session() {
    let Some(runtime) = runtime() else { return };
    let profile = format!(
        "pmt-test-{}",
        ulid::Ulid::generate().to_string().to_lowercase()
    );
    let config = ContainerConfig {
        persistent: true,
        mount_workspace: false,
        ..ContainerConfig::default()
    };
    let first = Session::open(runtime.clone(), "keep-1", config.clone(), &profile);
    let name = first.transport.container().name().to_owned();
    let _cleanup = Removed(&runtime, name.clone());
    let hostname = first.run("hostname").await;
    assert_eq!(hostname.exit_code, Some(0), "{hostname:?}");
    first.close().await;
    drop(first);
    let state = runtime
        .run(&[
            "inspect".into(),
            "--format={{.State.Running}}".into(),
            name.clone(),
        ])
        .unwrap();
    let state = String::from_utf8_lossy(&state.stdout);
    assert_eq!(state.trim(), "false", "stopped at session end, not removed");
    // Even an orphan reaper with no grace period leaves it alone.
    let reaped = pmt_sandbox::container::reap_orphans(&runtime, &profile, Duration::ZERO).unwrap();
    assert!(reaped.is_empty(), "{reaped:?}");
    let later = Session::open(runtime.clone(), "keep-2", config, &profile);
    assert_eq!(later.transport.container().name(), name);
    let again = later.run("hostname").await;
    assert_eq!(
        again.output.trim(),
        hostname.output.trim(),
        "the same container"
    );
    later.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hardening_is_in_force_inside_the_container() {
    let Some(runtime) = runtime() else { return };
    let session = Session::new(runtime, "hardening", ContainerConfig::default());
    let status = session
        .run("grep -E '^(NoNewPrivs|CapEff)' /proc/self/status")
        .await;
    assert!(
        status.output.contains("NoNewPrivs:\t1"),
        "{}",
        status.output
    );
    // Only CHOWN, DAC_OVERRIDE and FOWNER (bits 0, 1, 3) are kept.
    assert!(
        status.output.contains("CapEff:\t000000000000000b"),
        "{}",
        status.output
    );
    let root = session.run("touch /pmt-root-file").await;
    assert_ne!(root.exit_code, Some(0), "the root filesystem is read-only");
    let tmp = session.run("touch /tmp/x && mount | grep ' /tmp '").await;
    assert!(tmp.output.contains("nosuid"), "{}", tmp.output);
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn network_off() {
    let Some(runtime) = runtime() else { return };
    let config = ContainerConfig {
        network: false,
        ..ContainerConfig::default()
    };
    let session = Session::new(runtime, "network", config);
    let fetch = session
        .run("wget -q -T 5 -O /dev/null https://example.com")
        .await;
    assert_ne!(fetch.exit_code, Some(0), "{fetch:?}");
    let links = session.run("cat /proc/net/dev").await;
    assert!(!links.output.contains("eth0"), "{}", links.output);
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_state_and_the_workspace_persist_across_calls() {
    let Some(runtime) = runtime() else { return };
    let session = Session::new(runtime, "state", ContainerConfig::default());
    let first = session
        .run("mkdir -p sub && cd sub && export PMT_X=42 && echo hi > note.txt")
        .await;
    assert_eq!(first.exit_code, Some(0), "{first:?}");
    assert_eq!(first.cwd.as_deref(), Some("/workspace/sub"));
    let second = session.run("echo \"$PMT_X $(pwd)\"").await;
    assert_eq!(second.output.trim(), "42 /workspace/sub");
    let on_host = std::fs::read_to_string(session.fixture.work.join("sub/note.txt")).unwrap();
    assert_eq!(on_host, "hi\n");
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_stops_the_command_inside_the_container() {
    let Some(runtime) = runtime() else { return };
    let session = Session::new(runtime, "timeout", ContainerConfig::default());
    let mut args = TerminalArgs::new("sleep 300");
    args.timeout = Some(2.0);
    let result = session.terminal.run(args).await;
    assert_eq!(result.status, Status::Timeout);
    let mut left = String::new();
    for _ in 0..20 {
        left = session
            .run("ps -o args | grep -c '[s]leep 300' || true")
            .await
            .output;
        if left.trim() == "0" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(left.trim(), "0", "the remote command outlived its timeout");
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn container_runtime_down() {
    let Some(runtime) = runtime() else { return };
    let session = Session::new(runtime.clone(), "down", ContainerConfig::default());
    assert_eq!(session.run("true").await.exit_code, Some(0));
    let name = session.transport.container().name().to_owned();
    let removed = runtime.run(&["rm".into(), "--force".into(), name]).unwrap();
    assert!(removed.status.success());
    let degraded = session.run("true").await;
    assert_eq!(degraded.status, Status::Degraded, "{degraded:?}");
    assert!(degraded.retry_hint.is_some());
    let next = session.run("echo back").await;
    assert_eq!(next.output.trim(), "back");
    assert!(next.environment_recreated);
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_processes_run_and_stop_inside_the_container() {
    let Some(runtime) = runtime() else { return };
    let session = Session::new(runtime, "background", ContainerConfig::default());
    let mut args = TerminalArgs::new("echo started; sleep 300");
    args.background = true;
    let started = session.terminal.run(args).await;
    let id = started.process_id.clone().expect("a process id");
    let processes = session.terminal.processes();
    let kill =
        serde_json::from_value(serde_json::json!({"action": "kill", "process_id": id})).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = processes.handle(&kill).await;
    let mut left = String::new();
    for _ in 0..20 {
        left = session
            .run("ps -o args | grep -c '[s]leep 300' || true")
            .await
            .output;
        if left.trim() == "0" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(left.trim(), "0");
    session.close().await;
}

/// Granted host inputs (an attached file, a skill script) a command names
/// are uploaded on demand and the command runs on their copies (spec:
/// execution-environments, Remote file sync and sync-back); an unchanged
/// file is not sent again, a changed one is.
#[tokio::test]
async fn named_host_inputs_are_uploaded_on_demand() {
    use pmt_tools::terminal::inputs::Inputs;
    use std::os::unix::fs::PermissionsExt as _;

    let Some(runtime) = runtime() else { return };
    let config = ContainerConfig {
        mount_workspace: false,
        ..ContainerConfig::default()
    };
    let session = Session::new(runtime, "inputs", config);
    let media = session.fixture.profile.join("cache/media");
    let skills = session.fixture.profile.join("skills");
    let skill = skills.join("devops/report");
    std::fs::create_dir_all(skill.join("scripts")).unwrap();
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: report\n---\n").unwrap();
    std::fs::write(skill.join("data.txt"), "from the skill\n").unwrap();
    let script = skill.join("scripts/run.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat \"$(dirname \"$0\")/../data.txt\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let attached = media.join("invoice.txt");
    std::fs::write(&attached, "total 42\n").unwrap();
    let exec: Arc<dyn RemoteExec> = Arc::new(Arc::clone(&session.transport) as Arc<dyn Transport>);
    session.terminal.set_inputs(Arc::new(Inputs::new(
        "container",
        exec,
        "/tmp/.prometheus-inputs/test",
        &[media.clone(), skills.clone()],
    )));

    let ran = session.run(&script.display().to_string()).await;
    assert_eq!(ran.exit_code, Some(0), "{ran:?}");
    assert_eq!(ran.output.trim(), "from the skill");
    let read = session.run(&format!("cat {}", attached.display())).await;
    assert_eq!(read.output.trim(), "total 42", "{read:?}");
    // Nothing of the host path is left in what ran on the container.
    let echoed = session.run(&format!("echo {}", attached.display())).await;
    assert!(
        echoed.output.starts_with("/tmp/.prometheus-inputs/test/0/"),
        "{}",
        echoed.output
    );
    std::fs::write(&attached, "total 43, corrected\n").unwrap();
    let again = session.run(&format!("cat {}", attached.display())).await;
    assert_eq!(again.output.trim(), "total 43, corrected", "{again:?}");
    session.close().await;
}
