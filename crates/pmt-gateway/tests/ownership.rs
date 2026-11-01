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
