//! Whole-config checks of `[moa]` the schema cannot express (spec:
//! mixture-of-agents, Preset validation): presets do not nest (no slot on
//! the `moa` provider itself), every preset has references, temperatures
//! lie in 0.0-2.0 and `moa.default_preset` names a preset. Unknown slot
//! providers are refused where the provider registry is built.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, MoaPreset};

/// The virtual provider's names (presets may not point back at it).
const MOA_NAMES: [&str; 2] = ["moa", "mixture-of-agents"];

/// Checks `[moa]`.
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the offending key.
pub fn check(config: &Config) -> Result<(), Error> {
    let moa = &config.moa;
    for (name, preset) in &moa.presets {
        check_preset(&format!("moa.presets.{name}"), preset)?;
    }
    if moa.preset(&moa.default_preset).is_none() {
        return Err(invalid(
            "moa.default_preset",
            &format!(
                "'{}' is not a preset (presets: {})",
                moa.default_preset,
                moa.names().join(", ")
            ),
        ));
    }
    Ok(())
}

fn check_preset(key: &str, preset: &MoaPreset) -> Result<(), Error> {
    if preset.reference_models.is_empty() {
        return Err(invalid(
            &format!("{key}.reference_models"),
            "a preset needs at least one reference model",
        ));
    }
    if is_moa(&preset.aggregator.provider) {
        return Err(invalid(
            &format!("{key}.aggregator"),
            "the aggregator cannot be a Mixture of Agents preset (recursive MoA is not allowed)",
        ));
    }
    for (i, slot) in preset.reference_models.iter().enumerate() {
        if is_moa(&slot.provider) {
            return Err(invalid(
                &format!("{key}.reference_models[{i}]"),
                "a reference cannot be a Mixture of Agents preset (recursive MoA is not allowed)",
            ));
        }
    }
    let temperatures = [
        ("reference_temperature", preset.reference_temperature),
        ("aggregator_temperature", preset.aggregator_temperature),
    ];
    for (field, value) in temperatures {
        if let Some(t) = value.filter(|t| !(0.0..=2.0).contains(t)) {
            return Err(invalid(
                &format!("{key}.{field}"),
                &format!("{t} is outside 0.0-2.0"),
            ));
        }
    }
    Ok(())
}

fn is_moa(provider: &str) -> bool {
    MOA_NAMES.contains(&provider.trim().to_ascii_lowercase().as_str())
}

fn invalid(key: &str, why: &str) -> Error {
    Error::new(&codes::CONFIG_INVALID, format!("{key}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    const REFS: &str = "reference_models = [{provider = \"openrouter\", model = \"x\"}]\n";

    /// Scenario: Recursive aggregator refused (spec: mixture-of-agents,
    /// Preset validation).
    #[test]
    fn a_recursive_aggregator_is_refused_naming_the_key() {
        let err = check(&config(&format!(
            "[moa.presets.a]\n{REFS}aggregator = {{provider = \"moa\", model = \"b\"}}\n"
        )))
        .unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message().starts_with("moa.presets.a.aggregator:"),
            "{}",
            err.message()
        );
    }

    /// An unknown effort or an out-of-range temperature fails validation
    /// at load, naming its key (spec: mixture-of-agents, Preset validation).
    #[test]
    fn the_schema_refuses_bad_efforts_and_temperatures_by_key() {
        use crate::validate::{Node, document};
        let text = "[moa.presets.a]\n\
            reference_models = [{provider = \"x\", model = \"y\", reasoning_effort = \"huge\"}]\n\
            aggregator = {provider = \"x\", model = \"y\"}\nreference_temperature = 3.0\n";
        let doc: toml_edit::Document<String> = text.parse().unwrap();
        let issues = document(&Node::from_item(doc.as_item()));
        let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
        assert!(
            keys.iter()
                .any(|k| k.starts_with("moa.presets.a.reference_models")),
            "{issues:?}"
        );
        assert!(
            keys.contains(&"moa.presets.a.reference_temperature"),
            "{issues:?}"
        );
    }

    #[test]
    fn references_temperatures_and_the_default_preset_are_checked() {
        let agg = "aggregator = {provider = \"openrouter\", model = \"y\"}\n";
        check(&config(&format!("[moa.presets.a]\n{REFS}{agg}"))).unwrap();
        let empty = check(&config(&format!(
            "[moa.presets.a]\nreference_models = []\n{agg}"
        )));
        assert!(
            empty
                .unwrap_err()
                .message()
                .starts_with("moa.presets.a.reference_models:")
        );
        let hot = check(&config(&format!(
            "[moa.presets.a]\n{REFS}{agg}aggregator_temperature = 2.5\n"
        )));
        assert!(
            hot.unwrap_err()
                .message()
                .starts_with("moa.presets.a.aggregator_temperature:")
        );
        let nested = check(&config(&format!(
            "[moa.presets.a]\nreference_models = [{{provider = \"mixture-of-agents\", model = \"z\"}}]\n{agg}"
        )));
        assert!(
            nested
                .unwrap_err()
                .message()
                .starts_with("moa.presets.a.reference_models[0]:")
        );
        let missing = check(&config("[moa]\ndefault_preset = \"nope\"\n")).unwrap_err();
        assert!(missing.message().starts_with("moa.default_preset:"));
    }
}
