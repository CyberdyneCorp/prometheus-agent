//! UI languages from the command line (spec: i18n): `display.language`
//! selection, layered catalogs, language packs and their validation, and
//! the migration of Hermes language settings.

mod common;

use std::path::{Path, PathBuf};

use common::hermes::Hermes;
use common::{mock_home, ok, run, stderr, stdout, temp_home};

/// Every bundled language id.
const BUNDLED: [&str; 17] = [
    "en", "af", "ar", "de", "es", "fr", "ga", "hu", "it", "ja", "ko", "pt", "ru", "tr", "uk", "zh",
    "zh-hant",
];

fn profile(home: &Path) -> PathBuf {
    home.join("profiles/default")
}

/// A no-code language pack declaring Polish, in `dir`.
fn polish_pack(dir: &Path, catalog: &str) {
    std::fs::create_dir_all(dir.join("locales")).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"polski\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Polish UI strings.\"\nlicense = \"MIT\"\nrequires_prometheus = \">=0.1\"\n\
         worlds = []\n\n[[provides_locales]]\nid = \"pl\"\nendonym = \"Polski\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("locales/pl.toml"), catalog).unwrap();
}

/// Scenarios: Bundled language list; Unknown id refused.
#[test]
fn an_unknown_language_exits_78_naming_the_bundled_ids_and_changes_nothing() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["config", "set", "display.theme", "mono"]));
    let config = profile(home).join("config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    for id in ["xx", "pl"] {
        let out = run(home, &["config", "set", "display.language", id]);
        assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains(&format!(
                "Unknown language '{id}' for display.language. Available: "
            )),
            "{err}"
        );
        for bundled in BUNDLED {
            assert!(err.contains(bundled), "{bundled}: {err}");
        }
        assert!(err.contains("locales"), "{err}");
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    ok(&run(
        home,
        &["config", "set", "display.language", "zh-hant"],
    ));
    let value = ok(&run(home, &["config", "get", "display.language"]));
    assert!(value.contains("zh-hant"), "{value}");
}

#[test]
fn an_overlay_makes_its_language_selectable() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["config", "set", "display.theme", "mono"]));
    let locales = profile(home).join("locales");
    std::fs::create_dir_all(&locales).unwrap();
    std::fs::write(
        locales.join("eo.toml"),
        "[approval]\ndenied = \"Rifuzita\"\n",
    )
    .unwrap();
    ok(&run(home, &["config", "set", "display.language", "eo"]));
}

/// Scenario: Unknown key warned not failed.
#[test]
fn validating_a_pack_warns_about_unknown_keys_and_refuses_misfit_placeholders() {
    let home = temp_home();
    let pack = home.path().join("polski");
    polish_pack(
        &pack,
        "[approval]\ndenyed = \"Odrzucono\"\ndenied = \"Odrzucono\"\n",
    );
    let out = run(
        home.path(),
        &["plugins", "validate", pack.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("unknown key approval.denyed"), "{text}");
    assert!(text.contains("polski 1.0.0 (wasm) is valid"), "{text}");
    std::fs::write(
        pack.join("locales/pl.toml"),
        "[gateway]\nbusy_rejected = \"/{polecenie} nie teraz\"\n",
    )
    .unwrap();
    let out = run(
        home.path(),
        &["plugins", "validate", pack.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("gateway.busy_rejected uses placeholders"),
        "{}",
        stderr(&out)
    );
    std::fs::write(pack.join("locales/pl.toml"), "[approval]\ndenied = 3\n").unwrap();
    let out = run(
        home.path(),
        &["plugins", "validate", pack.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("value is not text"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: No-code pack adds a language (the selection half; the prompt
/// half is `i18n_chat`). A disabled pack's language is no longer
/// selectable, and a pack never selects its language itself.
#[test]
fn an_enabled_pack_makes_its_language_selectable_until_it_is_disabled() {
    let home = mock_home();
    let home = home.path();
    let dir = profile(home).join("plugins/polski");
    polish_pack(&dir, "[approval]\ndenied = \"Odrzucono\"\n");
    ok(&run(home, &["plugins", "adopt", "polski"]));
    ok(&run(home, &["plugins", "enable", "polski"]));
    let value = ok(&run(home, &["config", "get", "display.language"]));
    assert!(
        value.contains("en"),
        "the pack does not select itself: {value}"
    );
    ok(&run(home, &["config", "set", "display.language", "pl"]));
    ok(&run(home, &["config", "set", "display.language", "en"]));
    ok(&run(home, &["plugins", "disable", "polski"]));
    let out = run(home, &["config", "set", "display.language", "pl"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
}

/// Scenario: Env override becomes config. Overlays convert to TOML,
/// keeping text only.
#[test]
fn hermes_language_settings_and_overlays_migrate() {
    let tmp = temp_home();
    let home = tmp.path().join("prometheus");
    let h = Hermes::new(&tmp.path().join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\ndisplay:\n  language: de\n",
    );
    h.write("default", ".env", "HERMES_LANGUAGE=fr\n");
    h.write(
        "default",
        "locales/fr.yaml",
        "approval:\n  denied: \"Non !\"\n  count: 3\n",
    );
    let mut cmd = common::command(&home);
    cmd.args(["--json", "migrate", "from-hermes", "--hermes-home"])
        .arg(h.dir("default"))
        .arg("--yes")
        .stdin(std::process::Stdio::null());
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let config = std::fs::read_to_string(profile(&home).join("config.toml")).unwrap();
    let table: toml::Table = config.parse().unwrap();
    assert_eq!(
        table["display"]["language"].as_str(),
        Some("fr"),
        "{config}"
    );
    let overlay = std::fs::read_to_string(profile(&home).join("locales/fr.toml")).unwrap();
    assert!(overlay.contains("denied = \"Non !\""), "{overlay}");
    assert!(!overlay.contains("count"), "{overlay}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = report["profiles"][0]["entries"].as_array().unwrap();
    let item = |name: &str| {
        entries
            .iter()
            .find(|e| e["item"] == name)
            .unwrap_or_else(|| panic!("no {name}: {entries:#?}"))
    };
    assert_eq!(item(".env:HERMES_LANGUAGE")["target"], "display.language");
    assert_eq!(item("display.language")["disposition"], "skipped");
    assert_eq!(
        item("locales/fr.yaml:approval.count")["disposition"],
        "skipped"
    );
}
