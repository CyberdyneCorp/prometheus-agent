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
    spec=; dropped=
    for a; do
      case "$a" in
        --label=prometheus.spec=*) spec=${a#--label=prometheus.spec=} ;;
        --label=prometheus.dropped=*) dropped=${a#--label=prometheus.dropped=} ;;
      esac
    done
    echo "$spec|$dropped" > "$dir/labels"
    touch "$dir/exists"; echo id ;;
  inspect)
    for last; do :; done
    case "$*" in
      *"--format={{.State.Running}}|"*) [ -f "$dir/exists" ] || exit 1
         if [ -f "$dir/running" ]; then printf true; else printf false; fi
         echo "|$(cat "$dir/labels")" ;;
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
            // A workspace per session: persistent containers are shared
            // per workspace, and the pool is process-wide.
            workspace: Some(PathBuf::from(format!("/host/{session}"))),
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
        "--tmpfs=/tmp:rw,exec,nosuid,size=512m",
        "--tmpfs=/var/tmp:rw,noexec,nosuid,size=256m",
        "--pids-limit=2048",
        "--cpus=1",
        "--memory=5368709120b",
        "--label=prometheus.profile=p1",
        "--label=prometheus.session=s-hard",
        "--label=prometheus.persistent=true",
        "--network=none",
        "--volume=/host/s-hard:/workspace",
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
    // A stopped persistent container is reused by the next session on its
    // workspace, not an orphan.
    fs::write(fake.dir.join("ps"), "a1\nb2\nc3\nd4\n").unwrap();
    fs::write(
        fake.dir.join("inspect-d4"),
        "p1|false|2020-01-01T00:00:00Z|true",
    )
    .unwrap();
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
    let ephemeral = ContainerConfig {
        persistent: false,
        ..ContainerConfig::default()
    };
    let other = Pool::process().container(fake.spec("s-c", ephemeral, &[]));
    assert_eq!(other.name(), "pmt-p1-s-s-c");
}

#[test]
fn a_persistent_container_is_kept_between_sessions_on_its_workspace() {
    let fake = Fake::new();
    let on = |session: &str, workspace: Option<&str>| {
        let mut spec = fake.spec(session, ContainerConfig::default(), &[]);
        spec.profile = "keep".to_owned();
        spec.workspace = workspace.map(PathBuf::from);
        spec
    };
    let first = Pool::process().container(on("s-1", Some("/host/project")));
    first.ensure_running().unwrap();
    first.release(None);
    assert_eq!(fake.calls_of("stop").len(), 1, "stopped, not removed");
    assert!(fake.calls_of("rm").is_empty());
    let first_name = first.name().to_owned();
    drop(first);
    // A later session on the same workspace starts the same container.
    let later = Pool::process().container(on("s-2", Some("/host/project")));
    assert_eq!(later.name(), first_name);
    later.ensure_running().unwrap();
    assert_eq!(fake.calls_of("create").len(), 1, "{:#?}", fake.calls());
    assert_eq!(fake.calls_of("start").len(), 2);
    let elsewhere = Pool::process().container(on("s-3", Some("/host/other")));
    assert_ne!(elsewhere.name(), first_name);
    let unmounted = Pool::process().container(on("s-4", None));
    assert_eq!(unmounted.name(), "pmt-keep-p");
}

/// A session on the shared workspace `/host/reused`; the session id's
/// prefix names the profile, so tests never share a pooled handle.
fn reused(
    fake: &Fake,
    session: &str,
    config: ContainerConfig,
    allow: &[DegradedFeature],
) -> ContainerSpec {
    let mut spec = fake.spec(session, config, allow);
    session[..1].clone_into(&mut spec.profile);
    spec.workspace = Some(PathBuf::from("/host/reused"));
    spec
}

/// Runs a session's first command on the reused container and stops it
/// at session end, as a later process would see it (a fresh handle: no
/// in-memory state survives).
fn session_on(fake: &Fake, spec: ContainerSpec) -> Result<String, pmt_core::error::Error> {
    let container = Pool::process().container(spec);
    let mut result = Ok(());
    let logs = capture_warnings(|| result = container.ensure_running());
    result?;
    container.release(None);
    assert!(fake.dir.join("exists").exists());
    Ok(logs)
}

#[test]
fn a_reused_container_keeps_its_hardening_record() {
    let fake = Fake::new();
    fake.reject("no-new-privileges", "unknown flag: no-new-privileges");
    let nnp = [DegradedFeature::NoNewPrivileges];
    let warning = "container hardening 'no_new_privileges' unsupported by runtime";

    // Created without the flag under the waiver: WARN.
    let logs = session_on(
        &fake,
        reused(&fake, "r-1", ContainerConfig::default(), &nnp),
    )
    .unwrap();
    assert_eq!(logs.matches(warning).count(), 1, "{logs}");
    // A later process reuses it and still warns on that start.
    let logs = session_on(
        &fake,
        reused(&fake, "r-2", ContainerConfig::default(), &nnp),
    )
    .unwrap();
    assert_eq!(logs.matches(warning).count(), 1, "{logs}");
    assert_eq!(
        fake.calls_of("create").len(),
        2,
        "one rejected, one created"
    );
    assert!(fake.calls_of("rm").is_empty());

    // The waiver is gone: the container lacking the flag is not reused;
    // creating it again fails naming the flag and nothing starts.
    let starts = fake.calls_of("start").len();
    let err = session_on(&fake, reused(&fake, "r-3", ContainerConfig::default(), &[])).unwrap_err();
    assert_eq!(err.message(), warning);
    let rm = fake.calls_of("rm");
    assert_eq!(rm.len(), 1, "{rm:?}");
    assert!(rm[0].starts_with("rm --force pmt-r-w-"), "{rm:?}");
    assert_eq!(fake.calls_of("start").len(), starts);
    assert!(!fake.dir.join("exists").exists());
}

/// A handle of a session that ran under the waiver is still alive (another
/// session, or the pool reaper holding it for a tick) when a session
/// without the waiver asks for the same container: it must not inherit
/// the live handle and skip the hardening check.
#[test]
fn a_live_handle_does_not_carry_a_removed_waiver() {
    let fake = Fake::new();
    fake.reject("no-new-privileges", "unknown flag: no-new-privileges");
    let nnp = [DegradedFeature::NoNewPrivileges];
    let waived = Pool::process().container(reused(&fake, "h-1", ContainerConfig::default(), &nnp));
    waived.ensure_running().unwrap();

    let err = session_on(&fake, reused(&fake, "h-2", ContainerConfig::default(), &[])).unwrap_err();
    assert_eq!(
        err.message(),
        "container hardening 'no_new_privileges' unsupported by runtime"
    );
    drop(waived);
}

#[test]
fn a_reused_container_made_from_other_settings_is_recreated() {
    let fake = Fake::new();
    session_on(&fake, reused(&fake, "c-1", ContainerConfig::default(), &[])).unwrap();
    // Same settings: started again, not recreated.
    session_on(&fake, reused(&fake, "c-2", ContainerConfig::default(), &[])).unwrap();
    assert_eq!(fake.calls_of("create").len(), 1);
    assert!(fake.calls_of("rm").is_empty());
    // Network turned off later: the container is recreated with it.
    let offline = ContainerConfig {
        network: false,
        ..ContainerConfig::default()
    };
    session_on(&fake, reused(&fake, "c-3", offline, &[])).unwrap();
    assert_eq!(fake.calls_of("rm").len(), 1, "{:#?}", fake.calls());
    let creates = fake.calls_of("create");
    assert_eq!(creates.len(), 2);
    assert!(
        creates[1].split(' ').any(|a| a == "--network=none"),
        "{}",
        creates[1]
    );
    // A container made before the record labels existed is recreated too.
    fs::write(fake.dir.join("labels"), "|\n").unwrap();
    session_on(&fake, reused(&fake, "c-4", ContainerConfig::default(), &[])).unwrap();
    assert_eq!(fake.calls_of("rm").len(), 2);
    assert_eq!(fake.calls_of("create").len(), 3);
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
