//! The `whatsapp_cloud` catalog adapter in the real daemon (spec:
//! platform-adapters, `WhatsApp` Cloud API adapter): `prometheus gateway
//! whatsapp-cloud setup` generates and stores the verify token, and a
//! cron delivery to a user outside the 24-hour window leaves its outbox
//! entry failed with "`WhatsApp` 24-hour window closed".

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::catalog::{self, MockHttp, Reply, Request};
use common::gateway::{Daemon, gateway_home_with, wait};
use common::{ok, run};
use serde_json::{Value, json};

const PHONE_ID: &str = "106540352242922";
const USER: &str = "15551234567";

/// Graph refusing every message: the user is outside the window.
fn closed_window() -> MockHttp {
    MockHttp::start(Arc::new(|request: &Request| {
        if request.header("authorization") != Some("Bearer wa-token") {
            return Reply::status(401);
        }
        Reply::json(
            400,
            &json!({"error": {"message": "Re-engagement message", "code": 131_047}}),
        )
    }))
}

/// Installs the plugin into the home's default profile, keeping the
/// profile's own configuration.
fn install(home: &Path, graph: &MockHttp) {
    let config = home.join("profiles/default/config.toml");
    let mut before: toml::Table = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    let parent = catalog::scratch();
    let dir = catalog::plugin("whatsapp_cloud", parent.path(), true);
    let options = json!({"phone_number_id": PHONE_ID, "api_base": graph.base()});
    let kit = prometheus::plugins_cmd::adapter_plugin::PluginKit::install(
        home,
        &dir,
        &options,
        &[graph.host()],
    )
    .unwrap();
    catalog::secret(&kit, "WHATSAPP_CLOUD_ACCESS_TOKEN", "wa-token");
    catalog::secret(&kit, "WHATSAPP_CLOUD_APP_SECRET", "wa-app-secret");
    let added: toml::Table = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    merge(&mut before, added);
    std::fs::write(&config, before.to_string()).unwrap();
}

fn merge(into: &mut toml::Table, from: toml::Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(toml::Value::Table(a)), toml::Value::Table(b)) => merge(a, b),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

#[test]
fn setup_generates_and_stores_the_verify_token() {
    let graph = closed_window();
    let home = gateway_home_with("[[rule]]\nrepeat = true\ntext = \"hi\"\n", "");
    install(home.path(), &graph);
    let out = ok(&run(home.path(), &["gateway", "whatsapp-cloud", "setup"]));
    let token = out
        .lines()
        .find_map(|l| {
            l.strip_prefix("Verify token (stored as secret WHATSAPP_CLOUD_VERIFY_TOKEN): ")
        })
        .unwrap_or_else(|| panic!("{out}"))
        .to_owned();
    assert_eq!(token.len(), 64);
    assert!(out.contains("/p/default/whatsapp_cloud/"), "{out}");
    let stored = scope(home.path())
        .secrets()
        .resolve("WHATSAPP_CLOUD_VERIFY_TOKEN")
        .unwrap();
    assert_eq!(stored.expose(), token);
    // A second run replaces it.
    let again = ok(&run(home.path(), &["gateway", "whatsapp-cloud", "setup"]));
    assert!(!again.contains(&token));
}

/// Scenario: Window closed.
#[test]
fn a_cron_delivery_outside_the_window_fails_its_outbox_entry() {
    let graph = closed_window();
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"your daily report\"\n",
        "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\npreflight = false\n",
    );
    let home = home.path();
    install(home, &graph);
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.http]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    let added = json(
        home,
        &[
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "report",
            "--deliver",
            &format!("whatsapp_cloud:{USER}"),
            "--yes",
        ],
    );
    let id = added["job"]["id"].as_str().unwrap().to_owned();
    let store = pmt_store::Store::open(&scope(home)).unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    store
        .cron_modify_job(&id, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();
    let _daemon = Daemon::start(home);
    let mut failed = Value::Null;
    wait("the delivery fails", Duration::from_mins(2), || {
        let listed = json(home, &["gateway", "outbox", "list", "--state", "failed"]);
        failed = listed["entries"]
            .as_array()
            .and_then(|e| e.iter().find(|e| e["chat_id"] == USER).cloned())
            .unwrap_or(Value::Null);
        !failed.is_null()
    });
    let error = failed["last_error"].as_str().unwrap_or_default();
    assert!(error.contains("WhatsApp 24-hour window closed"), "{failed}");
}

fn scope(home: &Path) -> pmt_config::ProfileScope {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap()
}
