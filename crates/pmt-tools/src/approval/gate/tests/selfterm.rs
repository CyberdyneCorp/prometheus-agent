//! The self-termination guard through the real dispatcher (spec:
//! tool-system, Self-termination guard).

use super::*;
use crate::approval::selfterm::Daemon;

fn hosted(own: Vec<u32>) -> Options {
    Options {
        yolo: true,
        approvals: ApprovalsConfig {
            mode: ApprovalMode::Off,
            ..ApprovalsConfig::default()
        },
        hosted: Some((
            Daemon {
                pids: vec![std::process::id()],
                executables: vec!["prometheus".into()],
                command_lines: vec!["/usr/local/bin/prometheus daemon run".into()],
                services: vec!["prometheus-daemon.service".into()],
                run_dirs: vec!["/srv/pm/run".into()],
            },
            own,
        )),
        ..Options::default()
    }
}

#[tokio::test]
async fn pkill_under_yolo_is_blocked_in_a_daemon_session() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let session = session(&fx, channel, hosted(Vec::new())).await;
    for line in [
        "pkill -f prometheus",
        "pkill -f 'prometheus daemon'",
        "pkill -f bin/prometheus",
        "pkill -f 'daemon run'",
        "killall -r prom",
        "kill $(pgrep -f prometheus)",
        &format!("kill -9 {}", std::process::id()),
        "prometheus daemon restart",
        "systemctl --user stop prometheus-daemon",
    ] {
        let result = command(&session, line).await;
        assert_eq!(result.status, Status::Blocked, "{line}");
        assert!(
            result
                .text_content()
                .starts_with("BLOCKED (self-termination): "),
            "{line}: {}",
            result.text_content()
        );
    }
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
    assert!(prompts.try_next().is_none());
}

/// Indirect kills (pipelines, variables, the daemon's `run/` record) and
/// `execute_code` bodies are blocked under yolo too.
#[tokio::test]
async fn indirect_kills_and_code_under_yolo_are_blocked_in_a_daemon_session() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let session = session(&fx, channel, hosted(Vec::new())).await;
    for line in [
        "pgrep prometheus | xargs kill",
        "xargs kill < /srv/pm/run/gateway.json",
        "kill $(cat /srv/pm/run/gateway.json)",
        "p=$(pgrep prometheus); kill $p",
    ] {
        let result = command(&session, line).await;
        assert_eq!(result.status, Status::Blocked, "{line}");
    }
    let parent = format!("import os\nos.kill({}, 15)", std::process::id());
    for code in [
        r#"import os; os.system("pkill -f prometheus")"#,
        r#"import subprocess; subprocess.run(["pkill", "-f", "prometheus"])"#,
        "import os\nos.kill(os.getppid(), 9)",
        &parent,
    ] {
        let result = run(&session, "execute_code", json!({ "code": code }))
            .await
            .unwrap();
        assert_eq!(result.status, Status::Blocked, "{code}");
        assert!(
            result
                .text_content()
                .starts_with("BLOCKED (self-termination): "),
            "{code}: {}",
            result.text_content()
        );
    }
    let fine = run(&session, "execute_code", json!({"code": "print('hi')"}))
        .await
        .unwrap();
    assert_eq!(fine.status, Status::Ok);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
    assert!(prompts.try_next().is_none());
}

#[tokio::test]
async fn own_background_process_is_left_to_the_normal_gate() {
    let fx = fixture();
    let (channel, _prompts) = attached();
    let session = session(&fx, channel, hosted(vec![4242])).await;
    assert_eq!(command(&session, "kill 4242").await.status, Status::Ok);
    assert_eq!(command(&session, "kill 31337").await.status, Status::Ok);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sessions_outside_the_daemon_are_not_guarded() {
    let fx = fixture();
    let (channel, _prompts) = attached();
    let options = Options {
        hosted: None,
        ..hosted(Vec::new())
    };
    let session = session(&fx, channel, options).await;
    assert_eq!(
        command(&session, "pkill -f prometheus").await.status,
        Status::Ok
    );
}
