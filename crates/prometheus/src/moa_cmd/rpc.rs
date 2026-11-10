//! The daemon side of `moa.presets.*` (spec: mixture-of-agents, Preset
//! management commands): the same writes as `prometheus moa`, typed for
//! the dashboard and desktop editors.

use pmt_config::schema::{MoaFanout, MoaPreset, MoaSlot, ReasoningLevel};
use pmt_config::{Config, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_gateway::moa::Change;
use pmt_providers::moa::Fanout;
use pmt_rpc::contracts::{MoaPresetInput, MoaPresetList, MoaSlotInfo};

/// Applies `change` to `scope`'s presets.
///
/// # Errors
///
/// `rpc.invalid_params` for an unknown effort or cadence; the refusals of
/// [`super::put`] and [`super::delete`].
pub fn apply(scope: &ProfileScope, change: Change) -> Result<(), Error> {
    match change {
        Change::Put { name, preset } => super::put(scope, &name, &preset_of(preset)?),
        Change::Delete { name } => super::delete(scope, &name),
    }
}

/// The presets of `config` as the methods answer them.
///
/// # Errors
///
/// `rpc.internal` if the listing does not fit its contract (a bug).
pub fn list(config: &Config) -> Result<MoaPresetList, Error> {
    serde_json::from_value(super::presets_json(config))
        .map_err(|e| Error::new(&codes::RPC_INTERNAL, format!("moa.presets.list: {e}")))
}

fn preset_of(input: MoaPresetInput) -> Result<MoaPreset, Error> {
    let fanout = match input.fanout {
        Some(text) => {
            let fanout = MoaFanout::Text(text);
            Fanout::parse(&fanout).map_err(|bad| {
                invalid(&format!(
                    "fanout {bad} is not user_turn, per_iteration or every_n:N"
                ))
            })?;
            fanout
        }
        None => MoaFanout::default(),
    };
    Ok(MoaPreset {
        reference_models: input
            .references
            .into_iter()
            .map(slot_of)
            .collect::<Result<_, _>>()?,
        aggregator: slot_of(input.aggregator)?,
        reference_temperature: input.reference_temperature,
        aggregator_temperature: input.aggregator_temperature,
        fanout,
        enabled: input.enabled.unwrap_or(true),
    })
}

fn slot_of(slot: MoaSlotInfo) -> Result<MoaSlot, Error> {
    let reasoning_effort = slot
        .reasoning_effort
        .map(|effort| {
            ReasoningLevel::parse(&effort)
                .ok_or_else(|| invalid(&format!("unknown reasoning_effort '{effort}'")))
        })
        .transpose()?;
    Ok(MoaSlot {
        provider: slot.provider,
        model: slot.model,
        reasoning_effort,
    })
}

fn invalid(why: &str) -> Error {
    Error::new(
        &codes::RPC_INVALID_PARAMS,
        format!("moa.presets.put: {why}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(provider: &str, effort: Option<&str>) -> MoaSlotInfo {
        MoaSlotInfo {
            provider: provider.into(),
            model: "m".into(),
            reasoning_effort: effort.map(str::to_owned),
        }
    }

    #[test]
    fn editor_input_becomes_a_preset_or_a_refusal() {
        let input = MoaPresetInput {
            references: vec![slot("mock", Some("high"))],
            aggregator: slot("mock", None),
            reference_temperature: Some(0.2),
            aggregator_temperature: None,
            fanout: Some("per_iteration".into()),
            enabled: None,
        };
        let preset = preset_of(input.clone()).unwrap();
        assert_eq!(
            preset.reference_models[0].reasoning_effort,
            Some(ReasoningLevel::High)
        );
        assert!(preset.enabled);
        assert_eq!(preset.fanout, MoaFanout::Text("per_iteration".into()));
        let bad = MoaPresetInput {
            fanout: Some("sometimes".into()),
            ..input.clone()
        };
        assert_eq!(
            preset_of(bad).unwrap_err().code(),
            &codes::RPC_INVALID_PARAMS
        );
        let effort = MoaPresetInput {
            aggregator: slot("mock", Some("huge")),
            ..input
        };
        assert!(preset_of(effort).unwrap_err().message().contains("huge"));
    }

    #[test]
    fn the_listing_fits_its_contract() {
        let list = list(&Config::default()).unwrap();
        assert_eq!(list.default_preset, "default");
        assert_eq!(list.presets.len(), 1);
        assert!(list.presets[0].builtin && list.presets[0].default);
    }
}
