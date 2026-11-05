//! The local browser options of `[browser]` (spec: execution-environments,
//! Local browser options and headed mode).

use std::sync::{Arc, Mutex};

use pmt_config::schema::DegradedFeature;
use pmt_tools::browser::options::{Display, NO_DISPLAY_NOTICE};
use pmt_tools::result::Status;

use crate::support::{self, Fixture, run, site};

/// Scenario: Headless server.
#[tokio::test]
async fn headed_without_a_display_runs_headless_and_says_so_once() {
    let mut fixture = Fixture::new();
    fixture.display = Display::none();
    fixture.config.lock().unwrap().headed = true;
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()\nprint(js('navigator.userAgent'))").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(body["notice"], NO_DISPLAY_NOTICE);
    assert!(
        body["output"].as_str().unwrap().contains("Headless"),
        "{body}"
    );
    let (_, again) = run(&exec, "print(1)").await;
    assert!(again.get("notice").is_none(), "{again}");
    // A new browser session of the same process says nothing more.
    manager.close("ses_b/default").await;
    let (_, fresh) = run(&exec, "ensure_real_tab()").await;
    assert!(fresh.get("notice").is_none(), "{fresh}");
    manager.shutdown().await;
}

#[tokio::test]
async fn the_executable_and_extra_arguments_are_used() {
    let fixture = Fixture::new();
    {
        let mut config = fixture.config.lock().unwrap();
        config.executable_path = support::browsers()[0].display().to_string();
        config.chromium_args = vec!["--user-agent=PrometheusTest/1.0".to_owned()];
    }
    // No runtime and no host browser: only the configured path can start.
    let nothing = pmt_tools::browser::RuntimePolicy {
        runtimes: std::sync::Arc::new(pmt_tools::browser::NoRuntimes),
        allow_lazy_install: false,
        host_candidates: Vec::new(),
    };
    let manager = fixture.manager_with(nothing, std::collections::BTreeMap::new());
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()\nprint(js('navigator.userAgent'))").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(body["output"], "PrometheusTest/1.0\n");
    manager.shutdown().await;
}

#[tokio::test]
async fn arguments_that_weaken_isolation_need_their_downgrade() {
    let fixture = Fixture::new();
    fixture.config.lock().unwrap().chromium_args = vec!["--disable-web-security".to_owned()];
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Error);
    let text = result.text_content();
    assert!(
        text.contains(
            "sandbox unavailable: browser.chromium_args '--disable-web-security' turns off the browser's web security; set security.sandbox.allow_degraded = [\"web_security\"] to run without it"
        ),
        "{body}"
    );
    manager.shutdown().await;
}

/// An allowed downgrade is never silent: every browser start logs it at
/// WARN (spec: security-boundaries, Hardening downgrades are never silent).
#[tokio::test]
async fn an_allowed_downgrade_warns_on_every_browser_start() {
    let mut fixture = Fixture::new();
    fixture.allow_degraded = vec![DegradedFeature::WebSecurity];
    fixture.config.lock().unwrap().chromium_args = vec!["--disable-web-security".to_owned()];
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let warnings = Warnings::default();
    let _capture = warnings.capture();
    for start in 1..=2 {
        let (result, body) = run(&exec, "ensure_real_tab()\nprint(1)").await;
        assert_eq!(result.status, Status::Ok, "{body}");
        let text = warnings.text();
        assert_eq!(
            text.matches("browser degraded: browser.chromium_args '--disable-web-security'")
                .count(),
            start,
            "{text}"
        );
        assert!(text.contains("feature=\"web_security\""), "{text}");
        manager.close("ses_b/default").await;
    }
    manager.shutdown().await;
}

/// WARN lines logged on this thread while captured.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Warnings {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Warnings {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test]
async fn a_user_data_dir_keeps_the_profile_between_browsers() {
    let fixture = Fixture::new();
    fixture.config.lock().unwrap().user_data_dir = "browser/main".to_owned();
    let page = site().await;
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let store = format!(
        "new_tab('{}'); wait_for_load(); js(\"localStorage.setItem('kept', 'yes')\")",
        page.uri()
    );
    let (result, body) = run(&exec, &store).await;
    assert_eq!(result.status, Status::Ok, "{body}");
    manager.shutdown().await;
    let dir = fixture.profile.join("browser/main");
    assert!(dir.is_dir(), "the profile directory outlives the browser");

    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let read = format!(
        "new_tab('{}'); wait_for_load(); print(js(\"localStorage.getItem('kept')\"))",
        page.uri()
    );
    let (result, body) = run(&exec, &read).await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(body["output"], "yes\n");
    manager.shutdown().await;
    assert!(dir.is_dir());
}

#[tokio::test]
async fn a_browser_that_does_not_answer_fails_within_the_launch_timeout() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let silent = fixture.home.join("silent-browser");
    std::fs::write(&silent, "#!/bin/sh\nsleep 600\n").unwrap();
    std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o755)).unwrap();
    {
        let mut config = fixture.config.lock().unwrap();
        config.executable_path = silent.display().to_string();
        config.launch_timeout_secs = 1;
    }
    let manager = fixture.manager();
    let started = std::time::Instant::now();
    let refused = manager.session("ses_b/default").await.err().unwrap();
    assert!(refused.contains("did not start within 1 s"), "{refused}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

/// With a display, a headed browser opens a window (a non-headless user
/// agent) while still confined by the OS sandbox.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn headed_with_a_display_opens_a_window() {
    let mut fixture = Fixture::new();
    fixture.display = Display {
        available: true,
        env: Vec::new(),
    };
    fixture.config.lock().unwrap().headed = true;
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()\nprint(js('navigator.userAgent'))").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert!(body.get("notice").is_none(), "{body}");
    assert!(
        !body["output"].as_str().unwrap().contains("Headless"),
        "{body}"
    );
    manager.shutdown().await;
}
