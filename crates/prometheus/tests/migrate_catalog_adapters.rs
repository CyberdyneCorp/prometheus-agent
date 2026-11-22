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

/// The catalog plugin `id` published with a testing adapter.
fn publish(home: &Path, server: &CatalogServer, id: &'static str) {
    server.publish(
        home,
        &Published {
            id,
            crate_name: "testing-adapter",
            world: "platform-adapter@1.0",
            network: "chat.example.org",
            category: "platforms",
        },
    );
}

/// A Hermes profile configured by `.env` alone, as `hermes gateway setup`
/// leaves it.
fn env_only_hermes(root: &Path, env: &str) -> Hermes {
    let h = Hermes::new(&root.join("hermes"));
    h.write("default", "config.yaml", "_config_version: 52\n");
    h.write("default", ".env", env);
    h
}

/// The migrated `[gateway.platforms.<id>]` table.
fn platform_table(home: &Path, id: &str) -> toml::Value {
    let config: toml::Table = toml::from_str(
        &std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap(),
    )
    .unwrap();
    config["gateway"]["platforms"][id].clone()
}

fn secret_names(home: &Path) -> String {
    common::ok(&common::run(home, &["--json", "secrets", "list"]))
}

/// Hermes turns SMS on with `TWILIO_ACCOUNT_SID` (no `SMS_` credential):
/// the plugin is still installed, the table holds the Twilio settings and
/// the auth token by reference.
#[test]
fn an_env_only_hermes_sms_profile_gets_the_sms_plugin() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    publish(home.path(), &server, "sms");
    let h = env_only_hermes(
        home.path(),
        "TWILIO_ACCOUNT_SID=AC0123\nTWILIO_AUTH_TOKEN=twilio-secret-1\nTWILIO_PHONE_NUMBER=+15550001\nSMS_ALLOWED_USERS=+15550002\n",
    );
    let report = migrate(home.path(), &h.root);
    let installed = entry(&report, "integration:sms");
    assert_eq!(installed["disposition"], "transformed", "{installed}");
    assert!(
        home.path()
            .join("profiles/default/plugins/sms/plugin.toml")
            .is_file()
    );
    let table = platform_table(home.path(), "sms");
    assert_eq!(table["enabled"].as_bool(), Some(false));
    assert_eq!(table["token_secret"].as_str(), Some("TWILIO_AUTH_TOKEN"));
    assert_eq!(table["options"]["account_sid"].as_str(), Some("AC0123"));
    assert_eq!(table["options"]["phone_number"].as_str(), Some("+15550001"));
    assert_eq!(
        table["allow_users"].as_array().unwrap()[0].as_str(),
        Some("+15550002")
    );
    assert!(!table.to_string().contains("twilio-secret-1"));
    assert!(secret_names(home.path()).contains("TWILIO_AUTH_TOKEN"));
    common::ok(&common::run(home.path(), &["config", "validate"]));
}

/// Hermes's QQ Bot variables are `QQ_*`: the plugin is installed and the
/// client secret stored under the name the plugin reads.
#[test]
fn an_env_only_hermes_qq_profile_gets_the_qqbot_plugin() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    publish(home.path(), &server, "qqbot");
    let h = env_only_hermes(
        home.path(),
        "QQ_APP_ID=1020\nQQ_CLIENT_SECRET=qq-secret-1\nQQ_ALLOWED_USERS=openid-1\n",
    );
    let report = migrate(home.path(), &h.root);
    let installed = entry(&report, "integration:qqbot");
    assert_eq!(installed["disposition"], "transformed", "{installed}");
    assert!(
        home.path()
            .join("profiles/default/plugins/qqbot/plugin.toml")
            .is_file()
    );
    assert_eq!(
        entry(&report, ".env:QQ_CLIENT_SECRET")["target"],
        "QQBOT_CLIENT_SECRET"
    );
    let table = platform_table(home.path(), "qqbot");
    assert_eq!(table["enabled"].as_bool(), Some(false));
    assert_eq!(table["options"]["app_id"].as_str(), Some("1020"));
    assert_eq!(
        table["allow_users"].as_array().unwrap()[0].as_str(),
        Some("openid-1")
    );
    assert!(!table.to_string().contains("qq-secret-1"));
    let secrets = secret_names(home.path());
    assert!(secrets.contains("QQBOT_CLIENT_SECRET"), "{secrets}");
    common::ok(&common::run(home.path(), &["config", "validate"]));
}

/// A platform's settings without Hermes's enabling variable install
/// nothing and write no table (the two components agree).
#[test]
fn settings_without_the_enabling_variable_migrate_nothing() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    publish(home.path(), &server, "msgraph_webhook");
    let h = env_only_hermes(
        home.path(),
        "MSGRAPH_WEBHOOK_ENABLED=false\nMSGRAPH_WEBHOOK_CLIENT_STATE=state-1\n",
    );
    let report = migrate(home.path(), &h.root);
    assert!(
        report["profiles"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["item"] != "integration:msgraph_webhook"),
        "{report:#}"
    );
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(!config.contains("msgraph_webhook"), "{config}");
    common::ok(&common::run(home.path(), &["config", "validate"]));
}

/// The Graph listener's client state goes under the plugin's secret name,
/// which the table's `client_state_secret` names.
#[test]
fn the_graph_client_state_is_renamed_and_referenced() {
    let home = common::mock_home();
    let server = CatalogServer::start(home.path());
    publish(home.path(), &server, "msgraph_webhook");
    let h = env_only_hermes(
        home.path(),
        "MSGRAPH_WEBHOOK_ENABLED=true\nMSGRAPH_WEBHOOK_CLIENT_STATE=state-1\nMSGRAPH_WEBHOOK_ACCEPTED_RESOURCES=communications/onlineMeetings/*\n",
    );
    let report = migrate(home.path(), &h.root);
    assert_eq!(
        entry(&report, "integration:msgraph_webhook")["disposition"],
        "transformed"
    );
    let table = platform_table(home.path(), "msgraph_webhook");
    assert_eq!(
        table["options"]["client_state_secret"].as_str(),
        Some("MSGRAPH_CLIENT_STATE")
    );
    assert_eq!(
        table["options"]["accepted_resources"].as_array().unwrap()[0].as_str(),
        Some("communications/onlineMeetings/*")
    );
    assert!(secret_names(home.path()).contains("MSGRAPH_CLIENT_STATE"));
    common::ok(&common::run(home.path(), &["config", "validate"]));
}
