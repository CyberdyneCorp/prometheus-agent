use std::path::Path;

use semver::Version;

use super::*;

const BASE: &str = r#"
manifest_version = 1
id = "weather"
version = "1.0.0"
kind = "wasm"
description = "Forecasts."
license = "MIT"
requires_prometheus = ">=0.1"
worlds = ["tool@1.0"]
"#;

fn running() -> Version {
    Version::new(0, 1, 0)
}

fn parse(extra: &str) -> Result<Manifest, Error> {
    Manifest::parse(&format!("{BASE}{extra}"), &running())
}

#[test]
fn a_complete_manifest_parses() {
    let m = parse(
        r#"
requires_plugins = ["base"]
[capabilities]
network = ["api.weather.example", "*.cdn.example:443"]
secrets = ["WEATHER_KEY"]
hooks = ["pre_tool_call"]
llm = { enabled = true, allow_model_override = true, allowed_models = ["gpt-5"] }
prompt_section = 2000
"#,
    )
    .unwrap();
    assert_eq!(m.id, "weather");
    assert_eq!(m.kind, Kind::Wasm);
    assert_eq!(m.worlds[0].0.to_string(), "tool@1.0.0");
    let requested = m.capabilities.requested(Path::new("/d")).unwrap();
    let texts: Vec<String> = requested.iter().map(ToString::to_string).collect();
    for want in [
        "net.connect:api.weather.example",
        "net.connect:*.cdn.example:443",
        "secrets.read:WEATHER_KEY",
        "plugin.hook:pre_tool_call",
        "llm.call",
        "plugin.llm_model:gpt-5",
        "plugin.prompt_section:2000",
    ] {
        assert!(texts.contains(&want.to_owned()), "{want} in {texts:?}");
    }
}

#[test]
fn unknown_capability_key_fails_closed() {
    // Spec scenario "Unknown capability key fails closed".
    let err = parse("[capabilities]\nnetwrok = [\"api.example.com\"]\n").unwrap_err();
    assert_eq!(err.message(), "unknown capability 'netwrok'");
    assert_eq!(err.exit_code(), 65);
}

#[test]
fn newer_manifest_is_refused() {
    // Spec scenario "Newer manifest is refused".
    let text = BASE.replace("manifest_version = 1", "manifest_version = 2");
    let err = Manifest::parse(&text, &running()).unwrap_err();
    assert_eq!(
        err.message(),
        "requires a newer Prometheus (manifest_version 2)"
    );
}

#[test]
fn unknown_fields_kinds_and_worlds_are_named() {
    let err = parse("colour = \"red\"\n").unwrap_err();
    assert_eq!(err.message(), "unknown manifest field 'colour'");
    let err = Manifest::parse(&BASE.replace("\"wasm\"", "\"dylib\""), &running()).unwrap_err();
    assert!(err.message().starts_with("unknown plugin kind 'dylib'"));
    let err = Manifest::parse(&BASE.replace("tool@1.0", "toool@1.0"), &running()).unwrap_err();
    assert_eq!(err.message(), "unknown world 'toool'");
    let err = Manifest::parse(&BASE.replace("tool@1.0", "tool@7.0"), &running()).unwrap_err();
    assert!(
        err.message()
            .starts_with("world 'tool@7.0.0' is not supported")
    );
}

#[test]
fn a_range_excluding_this_version_is_refused() {
    let text = BASE.replace(">=0.1", ">=9.0");
    let err = Manifest::parse(&text, &running()).unwrap_err();
    assert_eq!(
        err.message(),
        "plugin 'weather' requires Prometheus >=9.0 (this is 0.1.0)"
    );
}

#[test]
fn hook_typos_and_bad_values_are_refused() {
    let err = parse("[capabilities]\nhooks = [\"pre_tool_cal\"]\n").unwrap_err();
    assert_eq!(err.message(), "unknown hook 'pre_tool_cal'");
    // messaging-gateway, Gateway lifecycle hooks are typed.
    let err = parse("[capabilities]\nhooks = [\"on_sesion_start\"]\n").unwrap_err();
    assert_eq!(err.message(), "unknown hook 'on_sesion_start'");
    assert!(parse("[capabilities]\nnetwork = [\"*\"]\n").is_err());
    assert!(parse("[capabilities]\nprompt_section = 5000\n").is_err());
    assert!(parse("[capabilities]\nsecrets = [\"lower\"]\n").is_err());
    assert!(parse("[capabilities]\nfs_read = [\"relative/x\"]\n").is_err());
    assert!(parse("[capabilities]\nllm = { enabled = true, temperature = 1 }\n").is_err());
}

#[test]
fn ids_follow_the_grammar() {
    for good in ["a", "weather-2", "x_y"] {
        check_id(good).unwrap();
    }
    for bad in ["", "-a", "Upper", "a.b", &"a".repeat(65)] {
        assert!(check_id(bad).is_err(), "{bad}");
    }
}

#[test]
fn labels_use_the_manifest_words() {
    let cap = |t: &str| pmt_core::capability::Capability::parse(t).unwrap();
    assert_eq!(
        label(&cap("secrets.read:GITHUB_TOKEN")),
        "secrets GITHUB_TOKEN"
    );
    assert_eq!(
        label(&cap("net.connect:exfil.example.net")),
        "network exfil.example.net"
    );
    assert_eq!(
        label(&cap("plugin.llm_model:gpt-5")),
        "llm model override gpt-5"
    );
    assert_eq!(
        describe(&cap("secrets.read:GITHUB_TOKEN")),
        "read the secret GITHUB_TOKEN"
    );
}

#[test]
fn a_language_pack_declares_its_locales_and_needs_no_code() {
    let m = Manifest::parse(
        &(BASE.replace("worlds = [\"tool@1.0\"]", "worlds = []")
            + "[[provides_locales]]\nid = \"pl\"\nendonym = \"Polski\"\n\n[[provides_locales]]\nid = \"fa\"\nrtl = true\n"),
        &running(),
    )
    .unwrap();
    assert!(m.is_data_only());
    assert_eq!(m.provides_locales.len(), 2);
    assert_eq!(m.provides_locales[0].endonym, "Polski");
    assert_eq!(m.provides_locales[1].endonym, "fa", "the id by default");
    assert!(m.provides_locales[1].rtl);
    for bad in [
        "[[provides_locales]]\nid = \"Polish\"\n",
        "[[provides_locales]]\nid = \"pl\"\nscript = \"x\"\n",
        "[[provides_locales]]\nid = \"pl\"\n[[provides_locales]]\nid = \"pl\"\n",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}
