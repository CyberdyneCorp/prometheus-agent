//! Library-level contracts of `pmt-update` that need a release server or
//! the installer scripts (spec: distribution-and-update).

use pmt_config::Home;
use pmt_config::daemon::UpdateConfig;
use pmt_update::flow::Install;
use pmt_update::manifest::ChannelIndex;
use pmt_update::{Channel, Identity, InstallReceipt, Owner, ReleaseVersion, Releases, TrustRoot};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = include_str!("fixtures/test.key");

fn sign(data: &[u8]) -> String {
    let sk = minisign::SecretKeyBox::from_string(TEST_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, None, None)
        .unwrap()
        .into_string()
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A self-owned install of `v1.4.0` in `dir`.
fn install(dir: &std::path::Path) -> Install {
    let binary = dir.join("bin").join("prometheus");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, b"binary").unwrap();
    let home = Home::open(&dir.join("home")).unwrap();
    InstallReceipt {
        owner: Owner::SelfInstall,
        channel: Channel::Stable,
        version: "v1.4.0".to_owned(),
        target: "x86_64-unknown-linux-musl".to_owned(),
        installed_at: "2026-10-31T00:00:00Z".to_owned(),
        binary: binary.clone(),
    }
    .write(home.root())
    .unwrap();
    Install {
        binary,
        home,
        version: ReleaseVersion::parse("v1.4.0").unwrap(),
        target: "x86_64-unknown-linux-musl".to_owned(),
    }
}

async fn server_with_stable(version: &str) -> MockServer {
    let server = MockServer::start().await;
    let index = serde_json::to_vec(&ChannelIndex::new(
        Channel::Stable,
        ReleaseVersion::parse(version).unwrap(),
    ))
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/channels/stable.json.minisig"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sign(&index)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/channels/stable.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(index))
        .mount(&server)
        .await;
    server
}

fn releases(server: &MockServer) -> Releases {
    Releases::new(
        Some(&server.uri()),
        Identity {
            version: "1.4.0",
            target: "x86_64-unknown-linux-musl",
        },
        &pmt_net::OutboundPolicy::default(),
        TrustRoot::embedded(),
        false,
    )
    .unwrap()
}

#[tokio::test]
async fn disabled_checks_make_no_request() {
    let dir = temp();
    let install = install(dir.path());
    let server = server_with_stable("v1.4.1").await;
    let config = UpdateConfig {
        check_interval_secs: 0,
        ..UpdateConfig::default()
    };
    let notice = pmt_update::notice::background(
        &install,
        &config,
        &releases(&server),
        jiff::Timestamp::now(),
    )
    .await
    .unwrap();
    assert_eq!(notice, None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn due_check_shows_a_notice_once_per_interval() {
    let dir = temp();
    let install = install(dir.path());
    let server = server_with_stable("v1.4.1").await;
    let config = UpdateConfig::default();
    let now = jiff::Timestamp::now();
    let releases = releases(&server);
    let notice = pmt_update::notice::background(&install, &config, &releases, now)
        .await
        .unwrap();
    assert_eq!(
        notice.as_deref(),
        Some("update available: v1.4.0 → v1.4.1; run: prometheus update")
    );
    let requests = server.received_requests().await.unwrap().len();
    let later = now + jiff::SignedDuration::from_hours(1);
    let again = pmt_update::notice::background(&install, &config, &releases, later)
        .await
        .unwrap();
    assert_eq!(again, notice, "the cached notice is shown again");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "no request within the interval"
    );
    assert!(
        install.binary.exists() && std::fs::read(&install.binary).unwrap() == b"binary",
        "a check never installs"
    );
}

#[tokio::test]
async fn unsigned_index_is_refused() {
    let dir = temp();
    let install = install(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/channels/stable.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let config = UpdateConfig::default();
    let quiet = |_: &str| {};
    let releases = releases(&server);
    let session = pmt_update::flow::Session {
        install: &install,
        config: &config,
        releases: &releases,
        progress: &quiet,
    };
    let err = pmt_update::flow::check(&session, None).await.unwrap_err();
    assert_eq!(
        err.message(),
        "signature verification failed for stable.json"
    );
    assert_eq!(err.exit_code(), 77);
}

#[cfg(unix)]
#[test]
fn installer_rejects_an_unknown_channel_before_downloading() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../distrib/install.sh");
    let out = std::process::Command::new("sh")
        .args([
            script,
            "--channel",
            "main",
            "--base-url",
            "http://127.0.0.1:9",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "install.sh: --channel must be stable or canary, not main"
    );
}
