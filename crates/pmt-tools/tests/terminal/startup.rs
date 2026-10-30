//! Shell startup files and the persistent shell state (spec:
//! execution-environments, Shell startup files).

use std::time::{Duration, Instant};

use pmt_config::schema::TerminalConfig;
use pmt_tools::terminal::shell::INIT_TIMEOUT_HINT;
use pmt_tools::terminal::{LocalTerminal, Status};

use crate::common::{Fixture, run};

/// The sandbox may read the rc files the test writes.
const READ_RC: &[&str] = &["fs.read:~/.bashrc", "fs.read:~/init/**"];

fn session(fixture: &Fixture, terminal: TerminalConfig) -> LocalTerminal {
    fixture.session_with(terminal, READ_RC)
}

#[tokio::test]
async fn bashrc_is_sourced_once_and_exports_persist() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.home.join(".bashrc"),
        "export FROM_RC=yes\nalias hello='echo hi'\n",
    )
    .unwrap();
    let session = session(&fixture, TerminalConfig::default());
    if !run(&session, "echo $BASH_VERSION")
        .await
        .output
        .trim()
        .is_empty()
    {
        assert_eq!(run(&session, "echo $FROM_RC").await.output, "yes\n");
        assert_eq!(run(&session, "hello").await.output, "hi\n");
    }
    run(&session, "export LATER=1; greet() { echo hey; }").await;
    assert_eq!(run(&session, "echo $LATER; greet").await.output, "1\nhey\n");
}

#[tokio::test]
async fn an_rc_file_reading_stdin_does_not_hang_a_command() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.home.join(".bashrc"),
        "read answer\nexport ASKED=done\n",
    )
    .unwrap();
    let session = session(&fixture, TerminalConfig::default());
    let started = Instant::now();
    let result = run(&session, "echo ran").await;
    assert_eq!(result.output, "ran\n");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stdin is closed during init"
    );
    assert_eq!(result.hint, None);
}

#[tokio::test]
async fn an_rc_file_that_blocks_on_read_times_out_into_a_plain_shell() {
    let fixture = Fixture::new();
    let fifo = fixture.home.join("init/never-written");
    std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let rc = fixture.home.join("init/blocking.sh");
    std::fs::write(
        &rc,
        format!("read line < {}\nexport BLOCKED=no\n", fifo.display()),
    )
    .unwrap();
    let terminal = TerminalConfig {
        shell_init_files: vec!["~/init/blocking.sh".to_owned()],
        shell_init_timeout_secs: 2,
        ..TerminalConfig::default()
    };
    let session = session(&fixture, terminal);
    let started = Instant::now();
    let first = run(&session, "echo [$BLOCKED]").await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(first.status, Status::Ok);
    assert_eq!(first.output, "[]\n", "the shell runs without init files");
    assert_eq!(first.hint.as_deref(), Some(INIT_TIMEOUT_HINT));
    assert_eq!(
        run(&session, "true").await.hint,
        None,
        "only the first result carries the hint"
    );
}

#[tokio::test]
async fn missing_init_files_are_skipped_with_a_one_time_notice() {
    let fixture = Fixture::new();
    let terminal = TerminalConfig {
        shell_init_files: vec!["~/init/absent.sh".to_owned()],
        ..TerminalConfig::default()
    };
    let session = session(&fixture, terminal);
    let first = run(&session, "true").await;
    assert_eq!(
        first.notices,
        ["terminal.shell_init_files: '~/init/absent.sh' not found; skipped"]
    );
    assert!(run(&session, "true").await.notices.is_empty());
}

#[test]
fn the_default_init_timeout_is_ten_seconds() {
    assert_eq!(TerminalConfig::default().shell_init_timeout_secs, 10);
    assert!(TerminalConfig::default().auto_source_bashrc);
}
