//! Timeouts, stalls, crashes, limits and shutdown (spec: python-bridge,
//! Call timeouts and stall detection; Crash isolation and restart policy;
//! Resource limits; Sidecar lifecycle; Registrations follow plugin-system
//! reload rules).

use std::time::{Duration, Instant};

use pmt_pybridge::supervisor::State;
use serde_json::json;

use crate::rig::{alive, eventually, quick, rig};

#[test]
fn a_call_past_its_bound_times_out_by_name() {
    let mut settings = quick();
    settings.call_timeout = Duration::from_millis(300);
    let rig = rig("echo", settings);
    let err = rig
        .supervisor
        .tool("slow", &json!({"seconds": 5}), &json!({}))
        .unwrap_err();
    assert_eq!(err.message(), "bridge call timed out after 300 ms");
}

#[test]
fn a_gil_stall_is_killed_after_three_missed_pings_and_restarted() {
    let rig = rig("echo", quick());
    let first = rig.supervisor.ensure().unwrap().pid();
    let started = Instant::now();
    let err = rig
        .supervisor
        .tool("stall", &json!({"seconds": 60}), &json!({}))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        err.message()
            .starts_with("bridge_crashed: plugin 'echo' exited (unresponsive: 3 pings unanswered)"),
        "{}",
        err.message()
    );
    assert!(eventually(Duration::from_secs(10), || {
        let status = rig.supervisor.status();
        status.state == State::Running && status.pid != Some(first)
    }));
    assert!(!alive(first));
}

#[test]
fn a_stall_is_killed_after_three_ping_periods_not_more() {
    // Pings keep their cadence while the sidecar is stalled: the first
    // unanswered ping goes out within one period of the stall, and the kill
    // follows three periods later. A ping that waits a full period before the
    // next sleep would take twice as long (6 to 8 periods).
    let every = Duration::from_secs(1);
    let mut settings = quick();
    settings.ping_every = every;
    let rig = rig("echo", settings);
    rig.supervisor.ensure().unwrap();
    let started = Instant::now();
    let err = rig
        .supervisor
        .tool("stall", &json!({"seconds": 60}), &json!({}))
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        err.message().contains("unresponsive: 3 pings unanswered"),
        "{}",
        err.message()
    );
    assert!(
        elapsed + Duration::from_millis(100) >= every * 3,
        "{elapsed:?}"
    );
    assert!(elapsed < every * 5, "{elapsed:?}");
}

#[test]
fn a_crash_loop_marks_the_plugin_failed_and_spares_the_rest() {
    let rig = rig("echo", quick());
    for _ in 0..5 {
        assert!(eventually(Duration::from_secs(10), || rig
            .supervisor
            .ensure()
            .is_ok()));
        let err = rig
            .supervisor
            .tool("crash", &json!({}), &json!({}))
            .unwrap_err();
        assert!(
            err.message()
                .starts_with("bridge_crashed: plugin 'echo' exited (signal 6 (SIGABRT))"),
            "{}",
            err.message()
        );
    }
    assert!(eventually(Duration::from_secs(5), || rig
        .supervisor
        .status()
        .state
        == State::Failed));
    let err = rig
        .supervisor
        .tool("echo", &json!({}), &json!({}))
        .unwrap_err();
    assert_eq!(
        err.message(),
        "plugin 'echo' is failed; see prometheus bridge logs echo"
    );
    let restarts = rig.supervisor.status().restarts;
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        rig.supervisor.status().restarts,
        restarts,
        "no restart after failing"
    );
    let log = std::fs::read_to_string(&rig.log).unwrap();
    assert!(log.contains("failed after 5 crashes"), "{log}");
    // `prometheus bridge restart` clears it.
    rig.supervisor.restart().unwrap();
    rig.supervisor.tool("echo", &json!({}), &json!({})).unwrap();
}

#[test]
fn a_memory_leak_is_terminated_naming_the_limit() {
    let mut settings = quick();
    settings.limits.max_memory_bytes = 128 * 1024 * 1024;
    let rig = rig("echo", settings);
    let err = rig
        .supervisor
        .tool("hog", &json!({}), &json!({}))
        .unwrap_err();
    assert!(
        err.message()
            .contains("resource limit exceeded (max_memory_bytes 128 MiB)"),
        "{}",
        err.message()
    );
}

#[test]
fn shutdown_reaps_the_process_tree_within_seven_seconds() {
    let rig = rig("echo", quick());
    let sidecar_pid = rig.supervisor.ensure().unwrap().pid();
    let child: u32 = rig
        .supervisor
        .tool("spawn", &json!({}), &json!({}))
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(child));
    let started = Instant::now();
    rig.supervisor.stop();
    assert!(
        started.elapsed() <= Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
    assert!(eventually(Duration::from_secs(1), || !alive(sidecar_pid)
        && !alive(child)));
    assert!(
        rig.data.join("unloaded").exists(),
        "on_unload callbacks ran"
    );
}

#[test]
fn a_restart_that_drops_a_tool_answers_by_name() {
    let rig = rig("echo", quick());
    rig.supervisor.ensure().unwrap();
    std::fs::write(rig.data.join("drop_tool"), "whoami").unwrap();
    rig.supervisor.ensure().unwrap().kill("test");
    assert!(eventually(Duration::from_secs(10), || {
        rig.supervisor
            .registrations()
            .is_some_and(|r| !r.has_tool("whoami"))
    }));
    let err = rig
        .supervisor
        .tool("whoami", &json!({}), &json!({}))
        .unwrap_err();
    assert_eq!(
        err.message(),
        "tool 'whoami' is no longer provided by plugin 'echo'"
    );
}

#[test]
fn an_idle_sidecar_without_hooks_is_stopped_and_started_again_on_use() {
    let mut settings = quick();
    settings.idle_shutdown = Some(Duration::from_millis(300));
    let rig = rig("lazy", settings);
    assert_eq!(
        rig.supervisor.tool("ping", &json!({}), &json!({})).unwrap(),
        "pong"
    );
    assert!(eventually(Duration::from_secs(5), || rig
        .supervisor
        .status()
        .state
        == State::Idle));
    assert_eq!(
        rig.supervisor.tool("ping", &json!({}), &json!({})).unwrap(),
        "pong"
    );
    assert_eq!(rig.supervisor.status().state, State::Running);
}

#[test]
fn the_environment_holds_only_the_allowed_variables_and_granted_secrets() {
    let rig = crate::rig::rig_with(
        "echo",
        quick(),
        vec![("SPOTIFY_TOKEN".to_owned(), "tok".to_owned())],
    );
    let answer: serde_json::Value = serde_json::from_str(
        &rig.supervisor
            .tool(
                "whoami",
                &json!({}),
                &json!({"cwd": "/w", "surface": "cli"}),
            )
            .unwrap(),
    )
    .unwrap();
    // macOS adds __CF_USER_TEXT_ENCODING to every process it starts.
    let env: Vec<&str> = answer["env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|v| *v != "__CF_USER_TEXT_ENCODING")
        .collect();
    assert_eq!(
        env,
        [
            "HOME",
            "LANG",
            "PATH",
            "PYTHONNOUSERSITE",
            "SPOTIFY_TOKEN",
            "TMPDIR"
        ]
    );
    assert_eq!(answer["openai"], serde_json::Value::Null);
    assert_eq!(answer["spotify"], "tok");
    assert_eq!(
        (answer["cwd"].as_str(), answer["surface"].as_str()),
        (Some("/w"), Some("cli"))
    );
    assert_eq!(
        rig.supervisor
            .tool("secret", &json!({"name": "SPOTIFY_TOKEN"}), &json!({}))
            .unwrap(),
        "tok"
    );
    let refused = rig
        .supervisor
        .tool("secret", &json!({"name": "OPENAI_API_KEY"}), &json!({}))
        .unwrap();
    assert!(
        refused.starts_with("PermissionError: secret 'OPENAI_API_KEY' is not granted"),
        "{refused}"
    );
}

#[test]
fn an_allocation_the_os_refuses_is_a_memory_limit_crash() {
    let mut settings = quick();
    settings.limits.max_memory_bytes = 128 * 1024 * 1024;
    let rig = rig("echo", settings);
    // Far beyond any machine: the allocator refuses it at once, as the
    // OS memory limit does for a smaller one.
    let err = rig
        .supervisor
        .tool("alloc", &json!({"bytes": 1_u64 << 62}), &json!({}))
        .unwrap_err();
    assert!(
        err.message()
            .contains("resource limit exceeded (max_memory_bytes 128 MiB)"),
        "{}",
        err.message()
    );
}

#[test]
fn a_child_that_left_the_process_group_is_reaped_with_the_sidecar() {
    let rig = rig("echo", quick());
    let child: u32 = rig
        .supervisor
        .tool("escape", &json!({}), &json!({}))
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(child));
    // Once a sample has seen it, it is a member of the tree.
    std::thread::sleep(pmt_pybridge::limits::SAMPLE_EVERY * 3);
    rig.supervisor.stop();
    assert!(
        eventually(Duration::from_secs(2), || !alive(child)),
        "pid {child} outlived its sidecar"
    );
}
