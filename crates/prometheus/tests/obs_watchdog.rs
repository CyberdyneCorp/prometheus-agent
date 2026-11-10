//! The daemon's runtime liveness watchdog and task dumps against real
//! daemon processes (spec: observability, Runtime liveness watchdog and
//! task dump).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use self::common::daemon;
use self::common::{mock_home, ok, run};

fn dumps(home: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(home.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("task-dump-"))
        })
        .collect()
}

/// Scenario: Stalled runtime restarts.
#[test]
fn a_stalled_runtime_is_dumped_marked_degraded_and_exits_75() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon.watchdog]\nprobe_interval_secs = 1\nmax_strikes = 3\n",
    )
    .unwrap();
    let mut child = common::command(home.path())
        .env("PMT_TEST_STALL_RUNTIME_MS", "1500")
        .args(["daemon", "run"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_mins(1);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the stalled daemon never exited");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(75));
    let dumps = dumps(home.path());
    assert_eq!(dumps.len(), 1, "{dumps:?}");
    let dump = std::fs::read_to_string(&dumps[0]).unwrap();
    assert!(dump.contains("reason: runtime_liveness_watchdog"), "{dump}");
    assert!(dump.contains("open spans"), "{dump}");
    // Every thread, also the watchdog's own (it never enters a span), and
    // the tasks blocking the workers, running where they were spawned.
    // (Linux truncates thread names to 15 bytes; this one fits.)
    assert!(dump.contains("\"pmt-watchdog\""), "{dump}");
    assert!(
        dump.lines().any(|l| l.contains("async task")
            && l.contains("watchdog.rs")
            && l.contains("RUNNING on thread")),
        "{dump}"
    );
    assert!(dump.contains("awaiting in "), "{dump}");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join("run/gateway.json")).unwrap())
            .unwrap();
    assert_eq!(record["state"], "degraded", "{record}");
    assert_eq!(record["exit_reason"], "runtime_liveness_watchdog");
    // `status` shows that record until a new daemon replaces it.
    let text = ok(&run(home.path(), &["status"]));
    assert!(text.contains("daemon: not running"), "{text}");
    assert!(
        text.contains("last daemon: degraded (runtime_liveness_watchdog)"),
        "{text}"
    );
    let again = ok(&run(home.path(), &["status"]));
    assert!(again.contains("last daemon: degraded"), "{again}");
    let _daemon = daemon::start(home.path(), &[]);
    assert!(!ok(&run(home.path(), &["status"])).contains("last daemon"));
}

/// Scenario: Dump without restart.
#[cfg(unix)]
#[test]
fn sigusr2_writes_a_dump_and_the_daemon_keeps_serving() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let home = mock_home();
    let running = daemon::start(home.path(), &[]);
    kill(
        Pid::from_raw(i32::try_from(running.pid).unwrap()),
        Signal::SIGUSR2,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while dumps(home.path()).is_empty() {
        assert!(Instant::now() < deadline, "no task dump after SIGUSR2");
        std::thread::sleep(Duration::from_millis(50));
    }
    let dump = std::fs::read_to_string(&dumps(home.path())[0]).unwrap();
    assert!(dump.contains("reason: requested (SIGUSR2)"), "{dump}");
    assert!(dump.contains("runtime:"), "{dump}");
    assert_eq!(
        run(home.path(), &["status", "--liveness"]).status.code(),
        Some(0)
    );
    assert_eq!(daemon::record(home.path()).process.pid, running.pid);
}

#[test]
fn debug_dump_tasks_writes_a_dump_over_the_control_channel() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let text = ok(&run(home.path(), &["debug", "dump-tasks"]));
    let path = text.trim().strip_prefix("task dump written to ").unwrap();
    let dump = std::fs::read_to_string(path).unwrap();
    assert!(
        dump.contains("reason: requested (debug dump-tasks)"),
        "{dump}"
    );
    assert_eq!(
        run(home.path(), &["status", "--liveness"]).status.code(),
        Some(0)
    );
}

#[test]
fn dump_tasks_without_a_daemon_says_so() {
    let home = mock_home();
    let out = run(home.path(), &["debug", "dump-tasks"]);
    assert_eq!(out.status.code(), Some(69), "{}", common::stderr(&out));
}

/// The requirement's stale-heartbeat warning: a frozen runtime cannot
/// answer its control channel, and that hung daemon is the one the
/// warning is for.
#[cfg(unix)]
#[test]
fn a_frozen_daemon_with_an_old_heartbeat_is_reported_stale() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let home = mock_home();
    let running = daemon::start(home.path(), &[]);
    let pid = Pid::from_raw(i32::try_from(running.pid).unwrap());
    kill(pid, Signal::SIGSTOP).unwrap();
    // Resume it whatever the asserts do, so the guard can stop it.
    let _resume = Resume(pid);
    let path = home.path().join("run/gateway.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let hour_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1);
    record["updated_at"] = hour_ago.to_string().into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let text = ok(&run(home.path(), &["status"]));
    assert!(text.contains("daemon: running (not answering)"), "{text}");
    assert!(text.contains("warning: daemon heartbeat stale ("), "{text}");
}

#[cfg(unix)]
struct Resume(nix::unistd::Pid);

#[cfg(unix)]
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGCONT);
    }
}
