//! The container backend's lifecycle against a scripted runtime CLI that
//! records every call (spec: execution-environments, Container hardening is
//! never silently dropped; Container lifecycle and orphan reaping;
//! Container backend options). The real-runtime behaviour is covered by
//! the pmt-tools container tests.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pmt_config::schema::{ContainerConfig, DegradedFeature};
use pmt_sandbox::container::{self, Activity, ContainerSpec, Pool, Runtime};

/// A fake `docker`: state lives in files next to it.
const FAKE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls.log"
case "$1" in
  info) cat "$dir/info" ;;
  create)
    if [ -f "$dir/reject.flag" ] && echo "$*" | grep -q -- "$(cat "$dir/reject.flag")"; then
      cat "$dir/reject.msg" >&2; exit 125
    fi
    touch "$dir/exists"; echo id ;;
  inspect)
    for last; do :; done
    case "$*" in
      *"{{.State.Running}} "*) [ -f "$dir/running" ] && echo true && exit 0
         [ -f "$dir/exists" ] && echo false && exit 0; exit 1 ;;
      *) cat "$dir/inspect-$last" ;;
    esac ;;
  start) touch "$dir/running" ;;
  stop) rm -f "$dir/running" ;;
  rm) rm -f "$dir/running" "$dir/exists" ;;
  ps) cat "$dir/ps" ;;
esac
"#;

struct Fake {
    _root: tempfile::TempDir,
    dir: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let dir = root.path().to_path_buf();
        let program = dir.join("docker");
        fs::write(&program, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(dir.join("info"), "[\"name=rootless\"]|Ubuntu").unwrap();
        fs::write(dir.join("calls.log"), "").unwrap();
        Self { _root: root, dir }
    }

    fn runtime(&self) -> Runtime {
        Runtime::at(self.dir.join("docker"), container::client_env())
    }

    fn reject(&self, flag: &str, message: &str) {
        fs::write(self.dir.join("reject.flag"), flag).unwrap();
        fs::write(self.dir.join("reject.msg"), message).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls.log"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn calls_of(&self, verb: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with(&format!("{verb} ")))
            .collect()
    }

    fn spec(
        &self,
        session: &str,
        config: ContainerConfig,
        allow: &[DegradedFeature],
    ) -> ContainerSpec {
        ContainerSpec {
            runtime: self.runtime(),
            profile: "p1".to_owned(),
            session: session.to_owned(),
            image: "alpine:3".to_owned(),
            config,
            allow_degraded: allow.to_vec(),
            workspace: Some(PathBuf::from("/host/work")),
            volumes: Vec::new(),
            user: None,
            idle_lifetime: Duration::from_mins(5),
        }
    }
}

#[test]
fn every_container_starts_with_the_full_hardening_and_labels() {
    let fake = Fake::new();
    let config = ContainerConfig {
        network: false,
        ..ContainerConfig::default()
    };
    let container = Pool::process().container(fake.spec("s-hard", config, &[]));
    container.ensure_running().unwrap();
    let create = &fake.calls_of("create")[0];
    for arg in [
        "--cap-drop=ALL",
        "--cap-add=CHOWN",
        "--cap-add=DAC_OVERRIDE",
        "--cap-add=FOWNER",
        "--security-opt=no-new-privileges",
        "--read-only",
        "--tmpfs=/tmp:rw,nosuid,size=512m",
        "--tmpfs=/var/tmp:rw,noexec,nosuid,size=256m",
        "--pids-limit=2048",
        "--cpus=1",
        "--memory=5368709120b",
        "--label=prometheus.profile=p1",
        "--label=prometheus.session=s-hard",
        "--network=none",
        "--volume=/host/work:/workspace",
    ] {
        assert!(
            create.split(' ').any(|a| a == arg),
            "{arg} missing from {create}"
        );
    }
    assert!(!create.contains("--cap-add=SYS"), "{create}");
    assert_eq!(fake.calls_of("start").len(), 1);
}

#[test]
fn runtime_rejects_no_new_privileges() {
    let fake = Fake::new();
    fake.reject(
        "no-new-privileges",
        "Error response from daemon: invalid --security-opt: \"no-new-privileges\" is not supported",
    );
    let container = Pool::process().container(fake.spec("s-nnp", ContainerConfig::default(), &[]));
    let err = container.ensure_running().unwrap_err();
    assert_eq!(
        err.message(),
        "container hardening 'no_new_privileges' unsupported by runtime"
    );
    assert!(fake.calls_of("start").is_empty(), "no container may start");
}

#[test]
fn an_allowed_rejection_drops_only_that_flag_and_warns_on_every_start() {
    let fake = Fake::new();
    fake.reject("no-new-privileges", "unknown flag: no-new-privileges");
    let spec = fake.spec(
        "s-allowed",
        ContainerConfig::default(),
        &[DegradedFeature::NoNewPrivileges],
    );
    let container = Pool::process().container(spec);
    let logs = capture_warnings(|| {
        container.ensure_running().unwrap();
        container.teardown();
        container.ensure_running().unwrap();
    });
    let creates = fake.calls_of("create");
    assert_eq!(creates.len(), 2, "{:#?}", fake.calls());
    assert!(!creates[1].contains("no-new-privileges"));
    assert!(creates[1].contains("--read-only"));
    assert_eq!(fake.calls_of("start").len(), 2);
    assert_eq!(
        logs.matches("container hardening 'no_new_privileges' unsupported by runtime")
            .count(),
        2,
        "{logs}"
    );
}

#[test]
fn only_a_rootless_or_vm_runtime_is_accepted_unless_all_is_allowed() {
    let fake = Fake::new();
    let strict = pmt_config::schema::SandboxConfig::default();
    assert!(fake.runtime().probe(&strict).unwrap().rootless);
    fs::write(
        fake.dir.join("info"),
        "[\"name=seccomp,profile=builtin\"]|Docker Desktop",
    )
    .unwrap();
    assert_eq!(
        fake.runtime().probe(&strict).unwrap().how,
        "Docker Desktop VM"
    );
    fs::write(
        fake.dir.join("info"),
        "[\"name=seccomp,profile=builtin\"]|Ubuntu 24.04",
    )
    .unwrap();
    let err = fake.runtime().probe(&strict).unwrap_err();
    assert!(err.message().contains("runs as root"), "{}", err.message());
    let all = pmt_config::schema::SandboxConfig {
        allow_degraded: vec![DegradedFeature::All],
        ..strict
    };
    assert!(!fake.runtime().probe(&all).unwrap().rootless);
}

#[test]
fn privileged_extra_arg() {
    let fake = Fake::new();
    let config = ContainerConfig {
        extra_args: vec!["--privileged".to_owned()],
        ..ContainerConfig::default()
    };
    let container = Pool::process().container(fake.spec("s-priv", config, &[]));
    let err = container.ensure_running().unwrap_err();
    assert!(
        err.message().contains("'--privileged'"),
        "{}",
        err.message()
    );
    assert!(fake.calls_of("create").is_empty());
}

#[test]
fn other_profiles_container() {
    let fake = Fake::new();
    fs::write(fake.dir.join("ps"), "a1\nb2\nc3\n").unwrap();
    fs::write(fake.dir.join("inspect-a1"), "p1|false|2020-01-01T00:00:00Z").unwrap();
    fs::write(fake.dir.join("inspect-b2"), "p2|false|2020-01-01T00:00:00Z").unwrap();
    let recent = jiff::Timestamp::now().to_string();
    fs::write(fake.dir.join("inspect-c3"), format!("p1|false|{recent}")).unwrap();
    let removed = container::reap_orphans(&fake.runtime(), "p1", Duration::from_mins(5)).unwrap();
    assert_eq!(removed, ["a1"]);
    let ps = &fake.calls_of("ps")[0];
    assert!(ps.contains("--filter=label=prometheus.profile=p1"), "{ps}");
    assert!(ps.contains("--filter=status=exited"), "{ps}");
    assert_eq!(fake.calls_of("rm"), ["rm a1"]);
}

struct Session {
    busy: std::sync::atomic::AtomicBool,
}

impl Activity for Session {
    fn busy(&self) -> bool {
        self.busy.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn idle_for(&self) -> Duration {
        Duration::from_hours(1)
    }
}

#[test]
fn long_foreground_command() {
    let fake = Fake::new();
    let mut spec = fake.spec("s-idle", ContainerConfig::default(), &[]);
    spec.idle_lifetime = Duration::from_millis(100);
    let container = Pool::process().container(spec);
    let session = Arc::new(Session { busy: true.into() });
    let weak: Weak<dyn Activity> = Arc::downgrade(&(Arc::clone(&session) as Arc<dyn Activity>));
    container.attach(weak);
    container.ensure_running().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(fake.calls_of("stop").is_empty(), "busy container torn down");
    session
        .busy
        .store(false, std::sync::atomic::Ordering::SeqCst);
    wait_for(|| !fake.calls_of("stop").is_empty());
    // Persistent by default: stopped, never removed.
    assert!(fake.calls_of("rm").is_empty());
    container.ensure_running().unwrap();
    assert_eq!(
        fake.calls_of("start").len(),
        2,
        "the next call starts it again"
    );
}

#[test]
fn a_non_persistent_container_is_removed_at_session_end() {
    let fake = Fake::new();
    let config = ContainerConfig {
        persistent: false,
        ..ContainerConfig::default()
    };
    let container = Pool::process().container(fake.spec("s-eph", config, &[]));
    container.ensure_running().unwrap();
    container.release(None);
    assert_eq!(fake.calls_of("rm").len(), 1);
}

#[test]
fn sessions_with_one_shared_key_share_a_container() {
    let fake = Fake::new();
    let config = ContainerConfig {
        shared_key: "team".to_owned(),
        ..ContainerConfig::default()
    };
    let a = Pool::process().container(fake.spec("s-a", config.clone(), &[]));
    let b = Pool::process().container(fake.spec("s-b", config, &[]));
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(a.name(), "pmt-p1-k-team");
    let other = Pool::process().container(fake.spec("s-c", ContainerConfig::default(), &[]));
    assert_eq!(other.name(), "pmt-p1-s-s-c");
}

fn wait_for(done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("condition not reached");
}

fn capture_warnings(f: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buf.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}
