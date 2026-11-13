//! The desktop window across two daemons (spec: desktop-app, Gateway
//! switching and scoping; Machine-wide UI preferences), end to end in the
//! desktop browser harness: the local daemon (`Laptop`) and another one
//! (`Homelab`) reached through its dashboard listener with a paired
//! device credential, plus a registered connection that is offline (`VPS`).
#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::dashboard::playwright;
use common::desktop::Setup;
use common::{append_config, daemon, dashboard, mock_home, ok, run};
use pmt_config::secrets::keychain::{Keychain as _, MemoryKeychain};
use pmt_desktop::connections::service::ConnectionsConfig;
use pmt_desktop::connections::ssh::SshPrograms;
use serde_json::{Value, json};

/// Stops a home's daemon when the test ends.
struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// "later" asks, after a few seconds, to run `rm -rf build`.
fn script(home: &Path) {
    let script = home.join("script.toml");
    std::fs::write(
        &script,
        "[[rule]]\ntool_result_regex = \".\"\ntext = \"done\"\nrepeat = true\n\
         [[rule]]\nuser = \"later\"\nlatency_ms = 4000\nrepeat = true\n[[rule.tool_calls]]\n\
         name = \"terminal\"\narguments = { command = \"rm -rf build\" }\n",
    )
    .unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            script.to_string_lossy()
        ),
    );
    std::fs::create_dir_all(home.join("profiles/default/workspace/build")).unwrap();
}

/// Scenario: Approval from a background connection. Scenario: Failed
/// switch is not remembered. Scenario: Remote connection keeps local
/// preferences.
#[test]
fn switching_connections_keeps_sessions_preferences_and_launch_choice_right() {
    let laptop = mock_home();
    let homelab = mock_home();
    script(laptop.path());
    script(homelab.path());
    let _homelab_daemon = daemon::start(homelab.path(), &[]);
    let _stop_homelab = Stop(homelab.path());
    let _stop_laptop = Stop(laptop.path());
    let port = dashboard::open(homelab.path(), &[]).port;
    let printed = ok(&run(
        homelab.path(),
        &["--json", "daemon", "pair-device", "laptop-app"],
    ));
    let token = serde_json::from_str::<Value>(&printed).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let offline = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let data = laptop.path().join("app-data");
    std::fs::create_dir_all(&data).unwrap();
    let registry = json!({
        "connections": [
            {"id": "local", "name": "Laptop", "kind": "local"},
            {"id": "homelab", "name": "Homelab", "kind": "remote", "url": format!("http://127.0.0.1:{port}")},
            {"id": "vps", "name": "VPS", "kind": "remote", "url": format!("http://127.0.0.1:{offline}")},
        ],
        "primary": "local",
    });
    std::fs::write(data.join("connections.json"), registry.to_string()).unwrap();
    let keychain = MemoryKeychain::default();
    keychain.set("desktop-connection/homelab", &token).unwrap();
    let harness = common::desktop::start_with(
        laptop.path(),
        Setup {
            connections: Some(ConnectionsConfig {
                dir: data,
                keychain: Arc::new(keychain),
                ssh: SshPrograms::default(),
            }),
            ..Setup::default()
        },
    );
    playwright(
        laptop.path(),
        "desktop-connections.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
    // The approval was answered on Homelab: its command ran there only.
    assert!(
        !homelab
            .path()
            .join("profiles/default/workspace/build")
            .exists()
    );
    assert!(
        laptop
            .path()
            .join("profiles/default/workspace/build")
            .exists()
    );
    // The accent chosen while Homelab was shown is this machine's.
    let local = std::fs::read_to_string(laptop.path().join("ui-prefs.json")).unwrap();
    assert!(local.contains("#123456"), "{local}");
    let remote = std::fs::read_to_string(homelab.path().join("ui-prefs.json")).unwrap_or_default();
    assert!(!remote.contains("#123456"), "{remote}");
}
