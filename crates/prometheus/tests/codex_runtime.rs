//! `prometheus codex-runtime` and `/codex-runtime` (spec: model-providers,
//! Codex app-server runtime; Codex runtime migration) against the fake
//! Codex CLI `pmt-codex-fixture`.

mod common;

use std::path::{Path, PathBuf};

use serde_json::Value;

use common::{codex_fixture, ok, run, stderr, temp_home, user_home};

fn config_path(home: &Path) -> PathBuf {
    home.join("profiles/default/config.toml")
}

fn write_config(home: &Path, text: &str) {
    let path = config_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn codex_config(home: &Path) -> PathBuf {
    user_home(home).join(".codex/config.toml")
}

fn log(codex: &Path) -> Vec<Value> {
    std::fs::read_to_string(codex.parent().unwrap().join("codex-fixture.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Scenario: Codex binary missing.
#[test]
fn turning_the_runtime_on_without_codex_is_refused_and_changes_nothing() {
    let home = temp_home();
    let missing = home.path().join("nowhere/codex");
    let before = format!("[model]\ncodex_bin = '{}'\n", missing.display());
    write_config(home.path(), &before);
    let out = run(home.path(), &["codex-runtime", "set", "on"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("model.codex_bin"), "{err}");
    assert_eq!(
        std::fs::read_to_string(config_path(home.path())).unwrap(),
        before
    );
    assert!(!codex_config(home.path()).exists());
}

/// Scenario: User text preserved (through the CLI, with the JSON report,
/// the enabled plugins and the callback server).
#[test]
fn migrate_twice_keeps_the_user_block_and_one_managed_section() {
    let home = temp_home();
    let codex = codex_fixture(&home.path().join("bin"));
    write_config(
        home.path(),
        &format!(
            "[model]\ncodex_bin = '{}'\n\n[mcp.servers.files]\ncommand = \"fs-mcp\"\nargs = [\"/srv\"]\nenv_secrets = [\"FS_TOKEN\"]\nlazy = true\n",
            codex.display()
        ),
    );
    let user = "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.mine]\ncommand = \"my-server\"\n";
    let target = codex_config(home.path());
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, user).unwrap();

    let dry: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "codex-runtime", "migrate", "--dry-run"],
    )))
    .unwrap();
    assert_eq!(dry["written"], false);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), user);

    let first: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "codex-runtime", "migrate"],
    )))
    .unwrap();
    let once = std::fs::read_to_string(&target).unwrap();
    ok(&run(home.path(), &["--json", "codex-runtime", "migrate"]));
    let twice = std::fs::read_to_string(&target).unwrap();
    assert_eq!(once, twice);
    assert!(twice.starts_with(user), "{twice}");
    assert_eq!(
        twice.matches("# >>> prometheus managed section").count(),
        1,
        "{twice}"
    );
    assert_eq!(
        twice.matches("# <<< prometheus managed section").count(),
        1,
        "{twice}"
    );
    for key in [
        "migrated",
        "preserved_user_servers",
        "skipped_keys_per_server",
        "errors",
        "target_path",
        "written",
    ] {
        assert!(first.get(key).is_some(), "{key} missing: {first}");
    }
    assert_eq!(first["migrated"], serde_json::json!(["files"]));
    assert_eq!(first["preserved_user_servers"], serde_json::json!(["mine"]));
    assert_eq!(
        first["skipped_keys_per_server"]["files"],
        serde_json::json!(["lazy"])
    );
    assert_eq!(first["written"], true);
    let parsed: toml::Table = twice.parse().unwrap();
    let servers = parsed["mcp_servers"].as_table().unwrap();
    assert_eq!(servers["files"]["env_vars"][0].as_str(), Some("FS_TOKEN"));
    let callback = servers["prometheus"]["args"].as_array().unwrap();
    let args: Vec<&str> = callback.iter().filter_map(toml::Value::as_str).collect();
    assert!(
        args.windows(3).any(|w| w == ["mcp", "serve", "--stdio"]),
        "{args:?}"
    );
    assert!(args.last().unwrap().contains("text_to_speech"), "{args:?}");
    // Installed and available plugins only.
    let plugins = parsed["plugins"].as_table().unwrap();
    assert_eq!(plugins.keys().collect::<Vec<_>>(), ["github"]);
    assert!(log(&codex).iter().any(|e| e["method"] == "plugin/list"));
    // Migration switched the runtime for new sessions.
    let config = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(
        config.contains("openai_runtime = \"codex_app_server\""),
        "{config}"
    );
}
