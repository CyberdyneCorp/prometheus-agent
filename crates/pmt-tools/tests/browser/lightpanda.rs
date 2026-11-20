//! The Lightpanda engine against a stand-in `lightpanda serve` (spec:
//! execution-environments, Lightpanda engine): one confined process per
//! browser session on a loopback port, reached over DevTools, recorded and
//! reaped like a Chromium.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::schema::{BrowserEngine, Config};
use pmt_tools::browser::launch::{InstallFuture, Runtimes};
use pmt_tools::browser::{Origin, RuntimePolicy};
use serde_json::json;

use crate::support::{Fixture, records};

/// A runtime store holding only the stand-in Lightpanda.
struct Installed(PathBuf);

impl Runtimes for Installed {
    fn installed(&self, name: &str) -> Option<PathBuf> {
        (name == "lightpanda").then(|| self.0.clone())
    }

    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a> {
        Box::pin(async move { Err(format!("{name} is not installable here")) })
    }
}

fn stand_in(fixture: &Fixture) -> PathBuf {
    let dir = fixture.home.join("runtimes/lightpanda/0.0.0-test");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("lightpanda");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-lightpanda.py"
        ),
        &exe,
    )
    .unwrap();
    exe
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test]
async fn a_session_runs_on_its_own_lightpanda_serve_process() {
    let fixture = Fixture::new();
    let exe = stand_in(&fixture);
    fixture.config.lock().unwrap().engine = BrowserEngine::Lightpanda;
    let manager = fixture.manager_with(
        RuntimePolicy {
            runtimes: Arc::new(Installed(exe)),
            allow_lazy_install: false,
            host_candidates: Vec::new(),
        },
        BTreeMap::new(),
    );
    let session = manager.session("ses_b/default").await.unwrap();
    assert!(matches!(session.origin(), Origin::Local));
    assert_eq!(session.profile(), Some("lightpanda"));
    let version = session
        .cdp()
        .call("Browser.getVersion", json!({}), None)
        .await
        .unwrap();
    assert_eq!(version["product"], "Lightpanda/fake");
    let argv: Vec<String> = serde_json::from_value(version["argv"].clone()).unwrap();
    assert_eq!(&argv[..4], ["serve", "--host", "127.0.0.1", "--port"]);
    assert!(argv[4].parse::<u16>().is_ok(), "{argv:?}");
    assert_eq!(records(&fixture.browser_cache()).len(), 1);

    // A second named session gets its own process.
    let other = manager.session("ses_b/second").await.unwrap();
    let pid = session.pid().unwrap();
    assert_ne!(other.pid(), Some(pid));

    manager.close("ses_b/default").await;
    assert!(!alive(pid), "closing the session ends its Lightpanda");
    manager.shutdown().await;
    assert!(records(&fixture.browser_cache()).is_empty());
}

#[test]
fn the_model_is_told_what_lightpanda_cannot_do_unless_it_is_shadowed() {
    let environments = Arc::new(pmt_tools::environments::Environments::default());
    let tools = pmt_tools::browser::tools(&environments);
    let exec = tools
        .iter()
        .find(|t| t.descriptor.name == "browser_exec")
        .unwrap();
    let describe = exec.describe.as_ref().unwrap();
    let mut config = Config::default();
    let plain = describe(&exec.descriptor.description, &config);
    assert!(!plain.contains("Lightpanda"));
    config.browser.engine = BrowserEngine::Lightpanda;
    let told = describe(&exec.descriptor.description, &config);
    assert!(
        told.contains("screenshots are unavailable") && told.contains("one page"),
        "{told}"
    );
    config.browser.provider = "browserbase".to_owned();
    assert_eq!(
        config.browser.engine_shadowed_by(),
        Some("browser.provider")
    );
    assert!(!describe(&exec.descriptor.description, &config).contains("Lightpanda"));
}

/// The real engine, when `PMT_TEST_LIGHTPANDA` names its executable (the
/// managed `lightpanda` runtime): it starts confined, answers over
/// DevTools through the sandbox and ends with its session.
#[tokio::test]
async fn the_real_lightpanda_starts_confined_when_available() {
    let Some(exe) = std::env::var_os("PMT_TEST_LIGHTPANDA").map(PathBuf::from) else {
        return;
    };
    let fixture = Fixture::new();
    fixture.config.lock().unwrap().engine = BrowserEngine::Lightpanda;
    let manager = fixture.manager_with(
        RuntimePolicy {
            runtimes: Arc::new(Installed(exe)),
            allow_lazy_install: false,
            host_candidates: Vec::new(),
        },
        BTreeMap::new(),
    );
    let session = manager.session("ses_b/default").await.unwrap();
    let version = session
        .cdp()
        .call("Browser.getVersion", json!({}), None)
        .await
        .unwrap();
    // Lightpanda answers as the Chrome version it emulates.
    assert!(
        version["product"].is_string() && version["jsVersion"].is_string(),
        "{version}"
    );
    let pid = session.pid().unwrap();
    manager.shutdown().await;
    assert!(!alive(pid));
}
