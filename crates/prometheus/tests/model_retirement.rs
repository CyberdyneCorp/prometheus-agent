//! The model retirement table and `prometheus config fix-models` (spec:
//! model-providers, Model retirement table; configuration-and-profiles,
//! Retired model rewrite).

mod common;

use std::path::{Path, PathBuf};

use common::{ok, run, run_with_stdin, stderr, stdout, temp_home};

fn config_path(home: &Path) -> PathBuf {
    home.join("profiles/default/config.toml")
}

fn write_config(home: &Path, text: &str) {
    let path = config_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn backups(home: &Path) -> Vec<String> {
    std::fs::read_dir(home.join("profiles/default"))
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.starts_with("config.toml.bak-"))
        .collect()
}

#[test]
fn a_retired_default_model_refuses_a_chat_naming_its_replacement() {
    let home = temp_home();
    write_config(home.path(), "[model]\ndefault = \"xai/grok-2\"\n");
    let out = run_with_stdin(home.path(), &["-z", "hello"], "");
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("xai/grok-4"), "{err}");
    assert!(err.contains("prometheus config fix-models"), "{err}");
}

#[test]
fn fix_models_is_a_dry_run_by_default() {
    let home = temp_home();
    let text = "# my settings\n[model]\nprovider = \"xai\"\ndefault = \"grok-2\"\n";
    write_config(home.path(), text);
    let out = ok(&run(home.path(), &["config", "fix-models"]));
    assert!(out.contains("model.default: grok-2 -> grok-4"), "{out}");
    assert!(out.contains("--apply"), "{out}");
    assert_eq!(
        std::fs::read_to_string(config_path(home.path())).unwrap(),
        text
    );
    assert!(backups(home.path()).is_empty());
}

#[test]
fn fix_models_apply_rewrites_every_slot_after_a_backup() {
    let home = temp_home();
    let text = "# keep this comment\n[model]\nprovider = \"xai\"\ndefault = \"grok-2\"\n\
                [auxiliary.title]\nprovider = \"anthropic\"\nmodel = \"claude-3-opus-20240229\"\n\
                [[fallback.chain]]\nprovider = \"xai\"\nmodel = \"grok-beta\"\n";
    write_config(home.path(), text);
    ok(&run(home.path(), &["config", "fix-models", "--apply"]));
    let written = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(written.contains("# keep this comment"), "{written}");
    let config: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(config["model"]["default"].as_str(), Some("grok-4"));
    assert_eq!(
        config["auxiliary"]["title"]["model"].as_str(),
        Some("claude-opus-4-1")
    );
    assert_eq!(
        config["fallback"]["chain"][0]["model"].as_str(),
        Some("grok-4")
    );
    let saved = backups(home.path());
    assert_eq!(saved.len(), 1, "{saved:?}");
    let backup = home.path().join("profiles/default").join(&saved[0]);
    assert_eq!(std::fs::read_to_string(backup).unwrap(), text);
    // Nothing is left to rewrite, and the doctor agrees.
    let again = ok(&run(home.path(), &["config", "fix-models"]));
    assert!(
        again.contains("no deprecated or retired model ids"),
        "{again}"
    );
}

#[test]
fn no_backup_skips_the_copy_and_the_doctor_warns_before_the_rewrite() {
    let home = temp_home();
    write_config(home.path(), "[model]\ndefault = \"xai/grok-2\"\n");
    let doctor = run(home.path(), &["--json", "doctor", "providers"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&doctor)).unwrap();
    let checks = report["checks"].as_array().unwrap();
    let retired = checks
        .iter()
        .find(|c| c["name"] == "retired_model")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(retired["status"], "warn");
    assert_eq!(retired["hint"], "prometheus config fix-models --apply");
    ok(&run(
        home.path(),
        &["config", "fix-models", "--apply", "--no-backup"],
    ));
    let written = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(written.contains("xai/grok-4"), "{written}");
    assert!(backups(home.path()).is_empty());
}
