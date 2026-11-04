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
                services: vec!["prometheus-daemon.service".into()],
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
