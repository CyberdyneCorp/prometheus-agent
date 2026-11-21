//! Catalog adapter set migration (spec: platform-adapters, Catalog adapter
//! set; hermes-migration, MCP servers and plugins): a Hermes profile using
//! a platform Prometheus serves through a catalog adapter plugin gets that
//! plugin installed from the signed catalog, disabled and without grants,
//! and its `[gateway.platforms.<p>]` table with secret references.

mod common;

use std::path::Path;
use std::process::{Output, Stdio};

use common::hermes::Hermes;
use common::plugin_catalog::{CatalogServer, Published};
use serde_json::Value;

fn migrate(home: &Path, hermes: &Path) -> Value {
    let out: Output = common::command(home)
        .args(["--json", "migrate", "from-hermes", "--yes", "--hermes-home"])
        .arg(hermes)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        common::stdout(&out),
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

fn mattermost_hermes(root: &Path) -> Hermes {
    let h = Hermes::new(&root.join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\nplatforms:\n  mattermost:\n    enabled: true\n",
    );
    h.write(
        "default",
        ".env",
        "MATTERMOST_TOKEN=mm-token-1\nMATTERMOST_ALLOWED_USERS=ana,bo\n",
    );
    h
}

/// Scenario: Migration installs adapter.
#[test]
fn a_hermes_mattermost_profile_gets_the_catalog_adapter_disabled_with_secret_refs() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    server.publish(
        home.path(),
        &Published {
            id: "mattermost",
            crate_name: "testing-adapter",
            world: "platform-adapter@1.0",
            network: "chat.example.org",
            category: "platforms",
        },
    );
    let h = mattermost_hermes(home.path());
    let report = migrate(home.path(), &h.root);
    let installed = entry(&report, "integration:mattermost");
    assert_eq!(installed["disposition"], "transformed", "{installed}");
    let plugin = home.path().join("profiles/default/plugins/mattermost");
    assert!(plugin.join("plugin.toml").is_file());
    // Disabled, no grants.
    let listed = common::ok(&common::run(home.path(), &["--json", "plugins", "list"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let mattermost = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "mattermost")
        .unwrap_or_else(|| panic!("{listed:#}"));
    assert_eq!(mattermost["state"], "disabled", "{mattermost}");
    assert!(
        !std::fs::read_to_string(home.path().join("profiles/default/config.toml"))
            .unwrap()
            .contains("[plugins.grants"),
    );
    // The platform table, disabled, naming the token by reference.
    let config: toml::Table = toml::from_str(
        &std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap(),
    )
    .unwrap();
    let table = &config["gateway"]["platforms"]["mattermost"];
    assert_eq!(table["enabled"].as_bool(), Some(false));
    assert_eq!(table["token_secret"].as_str(), Some("MATTERMOST_TOKEN"));
    assert_eq!(table["allow_users"].as_array().unwrap().len(), 2, "{table}");
    assert!(!table.to_string().contains("mm-token-1"));
    // The configuration (with the plugin's platform) still loads.
    common::ok(&common::run(home.path(), &["config", "validate"]));
}

/// Without the plugin in the catalog the integration stays `unsupported`
/// and no platform table is written.
#[test]
fn a_platform_the_catalog_lacks_is_reported_unsupported() {
    let home = common::mock_home();
    let h = mattermost_hermes(home.path());
    let report = migrate(home.path(), &h.root);
    assert_eq!(
        entry(&report, "integration:mattermost")["disposition"],
        "unsupported"
    );
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/mattermost")
            .exists()
    );
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(
        !config.contains("[gateway.platforms.mattermost]"),
        "{config}"
    );
}
