//! The M3 dashboard pages in a real browser (spec: web-dashboard): each
//! test sets up a home and its daemon, then runs one Playwright spec of
//! `web/e2e` against it (see `dashboard_e2e.rs` for the setup it needs).

#![cfg(feature = "dashboard")]

mod common;

use common::dashboard::playwright;
use common::{append_config, daemon, mock_home, ok, run};

fn webapp_home() -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[dashboard]\nport = 0\n[webapp]\nenabled = true\n",
    )
    .unwrap();
    home
}

/// Scenario: Opening from the CLI. Scenario: Sign out every tab.
#[test]
fn the_webapp_opens_from_the_cli_and_signs_out_every_tab() {
    let home = webapp_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "webapp.spec.ts", &[]);
}

/// Scenario: Override conflict. Scenario: Plugin frame reads the session
/// cookie.
#[test]
fn dashboard_plugins_render_in_sandboxed_frames() {
    let home = mock_home();
    common::dashboard_plugins::install(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "plugins.spec.ts", &[]);
}

/// Scenario: Secret shown once. Scenario: Offline docs.
#[test]
fn the_webhooks_and_docs_pages_work_in_the_browser() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "m3pages.spec.ts", &[]);
}

/// Scenario: Excluded path skipped (projects, Repository discovery), on
/// the dashboard's Projects page: discovered repositories are offered as
/// one-click project creation.
#[test]
fn the_projects_page_offers_discovered_repositories() {
    let home = mock_home();
    let code = common::user_home(home.path()).join("code");
    for repo in ["alpha", "beta", "gamma", "scratch/tmp"] {
        std::fs::create_dir_all(code.join(repo).join(".git")).unwrap();
    }
    ok(&run(
        home.path(),
        &[
            "projects",
            "create",
            "gamma",
            code.join("gamma").to_str().unwrap(),
        ],
    ));
    append_config(
        home.path(),
        &format!(
            "\n[projects]\nrepo_scan_enabled = true\nrepo_scan_roots = [{:?}]\n\
             repo_scan_exclude_paths = [\"~/code/scratch\"]\n",
            code.display().to_string()
        ),
    );
    let _daemon = daemon::start(home.path(), &[]);
    playwright(home.path(), "projects.spec.ts", &[]);
}

/// A foreground daemon, killed when dropped.
#[cfg(unix)]
struct Foreground(std::process::Child);

#[cfg(unix)]
impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A foreground daemon of `home` with `env` (the CLI's `daemon start`
/// does not pass test variables on).
#[cfg(unix)]
fn daemon_with(home: &std::path::Path, env: &[(&str, &str)]) -> Foreground {
    let child = common::command(home)
        .args(["daemon", "run"])
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !common::run(home, &["daemon", "status"]).status.success() {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Foreground(child)
}

/// Channel setup forms: Test and Restart adapter, and `WhatsApp` QR
/// pairing against a stand-in bridge.
#[cfg(unix)]
#[test]
fn the_channel_forms_test_restart_and_pair_in_the_browser() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = mock_home();
    let bridge = home.path().join("fake-bridge.sh");
    // The stand-in rotates its code and links only when the spec says so
    // (it creates `bridge-next` and `bridge-link` in the home), so the
    // page is checked at each step however slow the machine is.
    let gate = |name: &str| {
        format!(
            "while [ ! -e '{}' ]; do sleep 0.1; done\n",
            home.path().join(name).display()
        )
    };
    std::fs::write(
        &bridge,
        format!(
            "#!/bin/sh\necho '{{\"event\":\"qr\",\"qr\":\"2@first,abc\"}}'\n{}\
             echo '{{\"event\":\"qr\",\"qr\":\"2@second,def\"}}'\n{}\
             echo '{{\"event\":\"connected\",\"user\":{{\"name\":\"Ana\"}}}}'\n",
            gate("bridge-next"),
            gate("bridge-link"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bridge, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _daemon = daemon_with(
        home.path(),
        &[("PMT_TEST_WHATSAPP_BRIDGE", bridge.to_str().unwrap())],
    );
    playwright(home.path(), "adapters.spec.ts", &[]);
}
