//! Foreground runs (spec: execution-environments, Foreground cap,
//! promotion and server guard; Foreground result shaping; Spilled full
//! output is redacted and private; Infrastructure failures are degraded
//! results).

use std::time::{Duration, Instant};

use pmt_config::schema::{DegradedMode, TerminalConfig};
use pmt_tools::terminal::classify::SERVER_REFUSAL;
use pmt_tools::terminal::process::ProcessArgs;
use pmt_tools::terminal::{LocalTerminal, Status, TerminalArgs};

use crate::common::{Fixture, KNOWN_SECRET, run, with_notifications};

#[tokio::test]
async fn a_dev_server_in_the_foreground_is_refused() {
    let fixture = Fixture::new();
    let result = run(&fixture.session(), "npm run dev").await;
    assert_eq!(result.status, Status::Error);
    assert!(result.error.unwrap().starts_with(SERVER_REFUSAL));
}

#[tokio::test]
async fn a_long_timeout_is_promoted_to_a_tracked_background_process() {
    let fixture = Fixture::new();
    let (session, mut done) = with_notifications(&fixture, None);
    let args = TerminalArgs {
        timeout: Some(1800.0),
        ..TerminalArgs::new("sleep 1; echo finished")
    };
    let result = session.run(args).await;
    assert_eq!(result.status, Status::Started, "{result:?}");
    assert!(result.promoted_from_foreground);
    assert!(
        result
            .to_json()
            .to_string()
            .contains("promoted_from_foreground")
    );
    let id = result.process_id.unwrap();
    let completion = tokio::time::timeout(Duration::from_secs(30), done.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (completion.process_id.as_str(), completion.exit_code),
        (id.as_str(), 0)
    );
    let log = session
        .processes()
        .handle(&ProcessArgs {
            action: "log".to_owned(),
            process_id: Some(id),
            ..ProcessArgs::default()
        })
        .await;
    assert_eq!(log["output"], "finished\n");
}

#[tokio::test]
async fn a_trailing_ampersand_runs_in_the_background() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let started = Instant::now();
    let result = run(&session, "sleep 30 &").await;
    assert_eq!(result.status, Status::Started);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(session.processes().running(), [result.process_id.unwrap()]);
    session.close().await;
    assert!(session.processes().running().is_empty());
}

#[tokio::test]
async fn the_timeout_stops_the_whole_process_group() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let pid_file = fixture.work.join("child.pid");
    // The inner `&` is inside a quoted script: the foreground guard lets it
    // through, the timeout must still reap the grandchild.
    let command = format!("sh -c 'sleep 60 & echo $! > {}; wait'", pid_file.display());
    let args = TerminalArgs {
        timeout: Some(1.0),
        ..TerminalArgs::new(command)
    };
    let started = Instant::now();
    let result = session.run(args).await;
    assert_eq!(result.status, Status::Timeout, "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(result.hint.unwrap().contains("stopped after 1s"));
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone(pid).await, "grandchild {pid} survived the timeout");
}

/// Whether `pid` is gone (allowing the init process a moment to reap it).
pub(crate) async fn gone(pid: i32) -> bool {
    for _ in 0..50 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive || is_zombie(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn is_zombie(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_start()
                .starts_with('Z')
        })
}

#[tokio::test]
async fn oversized_output_keeps_head_and_tail_around_one_marker() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let command = "awk 'BEGIN { for (i = 0; i < 120000; i++) printf (i < 60000 ? \"h\" : \"t\") }'";
    let result = run(&session, command).await;
    assert_eq!(result.status, Status::Ok, "{:?}", result.error);
    let marker = "\n... [OUTPUT TRUNCATED - 70000 chars omitted out of 120000 total] ...\n";
    let (head, tail) = result.output.split_once(marker).expect("one marker");
    assert_eq!(head, "h".repeat(20_000));
    assert_eq!(tail, "t".repeat(30_000));
    assert!(result.truncated);
    let full = std::fs::read_to_string(result.full_output_path.unwrap()).unwrap();
    assert_eq!(full.len(), 120_000);
}

#[tokio::test]
async fn huge_output_spills_to_a_redacted_private_file() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let command = format!(
        "echo {KNOWN_SECRET}; printf '\\033[31mred\\033[0m\\n'; yes 'line of output' | head -n 400000"
    );
    let result = run(&session, &command).await;
    assert_eq!(result.status, Status::Ok, "{:?}", result.error);
    assert!(result.output.chars().count() < 50_200);
    let path = std::path::PathBuf::from(result.full_output_path.expect("spilled"));
    assert!(path.starts_with(fixture.profile.join("cache/terminal-spill/ses_1")));
    let full = std::fs::read_to_string(&path).unwrap();
    assert_eq!(full.lines().count(), 400_002);
    assert!(!full.contains(KNOWN_SECRET) && full.contains("[REDACTED]"));
    assert!(full.contains("red\n") && !full.contains('\u{1b}'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn output_is_redacted_and_exit_codes_are_explained() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let result = run(&session, &format!("echo {KNOWN_SECRET}")).await;
    assert_eq!(result.output, "[REDACTED]\n");
    let missing = run(&session, "definitely-not-a-command-xyz").await;
    assert_eq!(missing.exit_code, Some(127));
    assert_eq!(missing.status, Status::Error);
    assert_eq!(
        missing.exit_code_meaning.as_deref(),
        Some("command not found")
    );
}

#[tokio::test]
async fn a_broken_environment_is_degraded_and_recreated_on_the_next_call() {
    let fixture = Fixture::new();
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    let real_shell = setup.shell.path.clone();
    let shell = fixture.work.join("bin/bash");
    setup.shell.path = shell.clone();
    let session = LocalTerminal::new(setup).unwrap();

    let first = run(&session, "echo hi").await;
    assert_eq!(first.status, Status::Degraded, "{first:?}");
    assert_eq!((first.exit_code, first.output.as_str()), (Some(-1), ""));
    assert!(first.reason.unwrap().contains("is missing"));
    assert!(first.retry_hint.is_some());

    std::fs::create_dir_all(shell.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(real_shell, &shell).unwrap();
    let second = run(&session, "echo hi").await;
    assert_eq!(second.status, Status::Ok, "{second:?}");
    assert!(second.environment_recreated);
    assert!(!run(&session, "echo again").await.environment_recreated);
}

#[tokio::test]
async fn degraded_mode_fail_returns_an_error() {
    let fixture = Fixture::new();
    let terminal = TerminalConfig {
        degraded_mode: DegradedMode::Fail,
        ..TerminalConfig::default()
    };
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(terminal, env, fixture.sandbox(&[], None));
    setup.shell.path = fixture.work.join("missing-shell");
    let result = LocalTerminal::new(setup)
        .unwrap()
        .run(TerminalArgs::new("true"))
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(result.reason.is_some());
}
