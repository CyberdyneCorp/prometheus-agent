//! Catalog plugins for Home Assistant and Camofox in migration (spec:
//! plugin-system, Catalog plugins for Home Assistant and Camofox): a
//! Hermes profile browsing through Camofox gets the `camofox` catalog
//! plugin installed disabled without grants, its server URL in
//! `plugins.entries.camofox.url`, and the core `browser` toolset disabled.

mod common;

use std::path::Path;
use std::process::Stdio;

use common::hermes::Hermes;
use common::plugin_catalog::{CatalogServer, Published};
use serde_json::Value;

fn migrate(home: &Path, hermes: &Path) -> Value {
    let out = common::command(home)
        .args(["--json", "migrate", "from-hermes", "--yes", "--hermes-home"])
        .arg(hermes)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit {:?}\nstderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn entry<'a>(report: &'a Value, item: &str) -> &'a Value {
    report["profiles"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["item"] == item)
        .unwrap_or_else(|| panic!("no entry {item}: {report:#}"))
}

/// Scenario: Migrated Camofox profile.
#[test]
fn a_migrated_camofox_profile_gets_the_plugin_its_url_and_no_core_browser() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    server.publish(
        home.path(),
        &Published {
            id: "camofox",
            crate_name: "sample-tool",
            world: "tool@1.0",
            network: "localhost:9377",
            category: "browser",
        },
    );
    let h = Hermes::new(&home.path().join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\nbrowser:\n  cloud_provider: camofox\n",
    );
    h.write("default", ".env", "CAMOFOX_URL=http://127.0.0.1:9377\n");
    let report = migrate(home.path(), &h.root);
    let installed = entry(&report, "integration:camofox");
    assert_eq!(installed["disposition"], "transformed", "{installed}");
    assert!(
        home.path()
            .join("profiles/default/plugins/camofox/plugin.toml")
            .is_file()
    );
    let config: toml::Table = toml::from_str(
        &std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        config["plugins"]["entries"]["camofox"]["url"].as_str(),
        Some("http://127.0.0.1:9377"),
        "{config:#?}"
    );
    let disabled = config["tools"]["disabled_toolsets"].as_array().unwrap();
    assert!(
        disabled.iter().any(|t| t.as_str() == Some("browser")),
        "{disabled:?}"
    );
    assert!(!config.contains_key("grants"));
    let listed = common::ok(&common::run(home.path(), &["--json", "plugins", "list"]));
    assert!(listed.contains("\"camofox\""), "{listed}");
    common::ok(&common::run(home.path(), &["config", "validate"]));
}
