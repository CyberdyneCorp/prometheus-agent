//! One interactive owner per session (spec: cli, Single interactive owner
//! per session), and the session methods a CLI client needs to follow or
//! fork the session another client owns.

mod support;

use serde_json::json;

use self::support::Daemon;

#[tokio::test]
async fn a_second_exclusive_resume_names_the_owner_until_it_detaches() {
    let daemon = Daemon::start().await;
    let (first, _a) = daemon.client("default", false).await;
    let (second, _b) = daemon.client("default", false).await;
    let created = first
        .call("session.create", json!({"exclusive": true}))
        .await
        .unwrap();
    let session = created["session_id"].as_str().unwrap().to_owned();

    let err = second
        .call(
            "session.resume",
            json!({"session_id": session, "exclusive": true}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_004);
    assert_eq!(err.data.as_ref().unwrap().code, "session.in_use");
    assert_eq!(
        err.message,
        format!(
            "session {session} is open in another client (pid {})",
            std::process::id()
        )
    );
    // Following read-only and forking stay possible.
    second
        .call("session.attach", json!({"session_id": session}))
        .await
        .unwrap();
    let fork = second
        .call("session.fork", json!({"session_id": session}))
        .await
        .unwrap();
    assert_ne!(fork["session_id"], session.as_str());
    second
        .call(
            "session.resume",
            json!({"session_id": fork["session_id"], "exclusive": true}),
        )
        .await
        .unwrap();

    first
        .call("session.detach", json!({"session_id": session}))
        .await
        .unwrap();
    second
        .call(
            "session.resume",
            json!({"session_id": session, "exclusive": true}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_closed_owner_releases_the_session() {
    let daemon = Daemon::start().await;
    let (first, _a) = daemon.client("default", false).await;
    let session = first
        .call("session.create", json!({"exclusive": true}))
        .await
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    first.close();
    first.wait_closed().await;
    let (second, _b) = daemon.client("default", false).await;
    second
        .call(
            "session.resume",
            json!({"session_id": session, "exclusive": true}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn interrupt_needs_the_session_attached() {
    let daemon = Daemon::start().await;
    let (owner, _a) = daemon.client("default", false).await;
    let (other, _b) = daemon.client("default", false).await;
    let session = owner.call("session.create", json!({})).await.unwrap()["session_id"].clone();
    owner
        .call("session.interrupt", json!({"session_id": session}))
        .await
        .unwrap();
    let err = other
        .call("session.interrupt", json!({"session_id": session}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32_002);
}

/// A ticket request for `default` claiming `pid`.
#[cfg(unix)]
async fn ticket_claiming(daemon: &Daemon, pid: u32) -> Result<String, String> {
    use pmt_rpc::control::{self, ControlRequest, Purpose};
    let request = ControlRequest::Ticket {
        purpose: Purpose::Interactive,
        profiles: vec!["default".into()],
        pid: Some(pid),
    };
    let response = control::call(&daemon.endpoint.control, &request)
        .await
        .unwrap();
    control::ticket_of(response)
        .map(|t| t.ticket)
        .map_err(|e| e.message().to_owned())
}

/// `prometheus tui --standalone` serves only the TUI that runs it: the
/// peer's pid comes from the kernel, so another process claiming the
/// TUI's pid is refused, and the claim of the right process is ignored.
#[cfg(unix)]
#[tokio::test]
async fn only_the_kernel_reported_pid_gets_a_standalone_ticket() {
    let tui_pid = std::process::id() + 1;
    let mut options = support::options();
    options.only_pid = Some(tui_pid);
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    let err = ticket_claiming(&daemon, tui_pid).await.unwrap_err();
    assert!(
        err.contains(&format!("serves only the TUI that runs it (pid {tui_pid})")),
        "{err}"
    );
    let _ = daemon.stop().await;

    let mut options = support::options();
    options.only_pid = Some(std::process::id());
    let daemon = Daemon::start_with(options, tempfile::tempdir().unwrap()).await;
    assert!(ticket_claiming(&daemon, 1).await.is_ok());
    let _ = daemon.stop().await;
}
