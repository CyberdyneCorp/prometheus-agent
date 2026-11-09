//! Connecting to the user's own browser (spec: execution-environments,
//! Connecting to the user's browser).

use pmt_tools::browser::Origin;
use pmt_tools::browser::connect::{Starter, Status, resolve};
use pmt_tools::result::Status as ResultStatus;

use crate::support::{self, External, Fixture, run};

/// Scenario: Disconnect keeps the browser.
#[tokio::test]
async fn disconnecting_keeps_the_browser_and_returns_to_the_provider() {
    let fixture = Fixture::new();
    let executable = support::browsers()[0].clone();
    let mut external = External::start(&executable, &fixture.home.join("external"));
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    manager.set_connect("ses_b", Some(external.http()));
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, ResultStatus::Ok, "{body}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Connected(external.http()));
    drop(session);

    // `/browser disconnect`.
    manager.set_connect("ses_b", None);
    manager.close_prefixed("ses_b/").await;
    assert!(
        external.running(),
        "disconnecting closed the user's browser"
    );
    let status = Status::read(&external.http()).await.unwrap();
    assert!(!status.version.is_empty());

    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, ResultStatus::Ok, "{body}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Local, "the configured provider");
    drop(session);
    manager.shutdown().await;
    assert!(external.running());
    external.stop();
}

#[tokio::test]
async fn status_shows_the_endpoint_version_and_targets() {
    let fixture = Fixture::new();
    let executable = support::browsers()[0].clone();
    let external = External::start(&executable, &fixture.home.join("external"));
    let status = Status::read(&external.http()).await.unwrap();
    assert_eq!(status.endpoint, external.http());
    assert!(status.version.contains('/'), "{}", status.version);
    assert!(status.targets.iter().any(|(kind, _, _)| kind == "page"));
    let text = status.render();
    assert!(
        text.contains(&external.http()) && text.contains(&status.version),
        "{text}"
    );
    // A ws:// endpoint is used as given.
    assert!(Status::read(&external.endpoint).await.is_ok());
    external.stop();
}

/// `/browser connect` without a URL starts a browser on the debugging
/// port with a dedicated profile under `cache/browser/connect/` when
/// nothing listens there, and binds to it.
#[tokio::test]
async fn connect_without_a_url_starts_a_browser_with_a_dedicated_profile() {
    let fixture = Fixture::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let starter = Starter {
        port,
        ..Starter::new(support::browsers(), fixture.browser_cache(), false)
    };
    let endpoint = resolve(None, &starter).await.unwrap();
    assert_eq!(endpoint, format!("http://127.0.0.1:{port}"));
    assert!(
        starter
            .profile_dir()
            .starts_with(fixture.browser_cache().join("connect"))
    );
    let status = Status::read(&endpoint).await.unwrap();
    assert!(!status.version.is_empty());
    // Connecting again reuses the running browser.
    assert_eq!(resolve(None, &starter).await.unwrap(), endpoint);
    // The test ends the browser it started (a user's stays running).
    let cdp = pmt_tools::browser::cdp::Cdp::connect(
        &pmt_tools::browser::connect::websocket_url(&endpoint)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let _ = cdp.call("Browser.close", serde_json::json!({}), None).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while Status::read(&endpoint).await.is_ok() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn an_unreachable_endpoint_is_named() {
    let starter = Starter::new(Vec::new(), std::env::temp_dir(), false);
    let refused = resolve(Some("http://127.0.0.1:9"), &starter)
        .await
        .unwrap_err();
    assert!(refused.contains("127.0.0.1:9"), "{refused}");
    let scheme = resolve(Some("ftp://127.0.0.1:9"), &starter)
        .await
        .unwrap_err();
    assert!(scheme.contains("ftp"), "{scheme}");
}
