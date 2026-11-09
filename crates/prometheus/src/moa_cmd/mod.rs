//! `prometheus moa list|configure|delete` (spec: mixture-of-agents, Preset
//! management commands; MoA billing and accounting). `list` marks the
//! aggregator as the model that acts and is billed and lists the
//! references with their cadence; `configure` creates or updates a preset
//! with the model picker for each slot; `delete` refuses the preset
//! `moa.default_preset` names. Edits reach model calls of sessions that
//! start after them; a call in flight is never altered. The RPC methods
//! `moa.presets.*` share these functions.

mod configure;
pub mod rpc;

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_config::schema::{MoaFanout, MoaPreset, MoaSlot};
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use crate::context::Context;
use crate::output::{document, print, print_raw};

/// `prometheus moa` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum MoaCommand {
    /// List the presets: the aggregator acts and is billed, the
    /// references advise on their cadence.
    List,
    /// Create or update a preset with the model picker (default: the
    /// preset `moa.default_preset` names).
    Configure {
        /// The preset name.
        name: Option<String>,
    },
    /// Delete a configured preset (never the default one).
    Delete {
        /// The preset name.
        name: String,
    },
}

/// Runs one `moa` subcommand.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &MoaCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        MoaCommand::List => list(ctx),
        MoaCommand::Configure { name } => configure::run(ctx, name.as_deref()),
        MoaCommand::Delete { name } => {
            let scope = ctx.scope()?;
            delete(&scope, name)?;
            print(&format!(
                "deleted preset '{name}'; new sessions no longer offer moa/{name}"
            ))
        }
    }
}

/// The spelling of a preset's cadence.
fn fanout(fanout: &MoaFanout) -> String {
    match fanout {
        MoaFanout::Text(text) => text.clone(),
        MoaFanout::Table { mode, n } => match n {
            Some(n) => format!("{mode}:{n}"),
            None => mode.clone(),
        },
    }
}

fn slot(slot: &MoaSlot) -> Value {
    json!({
        "provider": slot.provider,
        "model": slot.model,
        "reasoning_effort": slot.reasoning_effort.map(pmt_config::schema::ReasoningLevel::as_str),
    })
}

/// Every preset of `scope`'s config as the `moa list` document and the
/// `moa.presets.list` result show it.
#[must_use]
pub fn presets_json(config: &pmt_config::Config) -> Value {
    let moa = &config.moa;
    let presets: Vec<Value> = moa
        .all()
        .into_iter()
        .map(|(name, preset)| {
            json!({
                "name": name,
                "default": name == moa.default_preset,
                "builtin": !moa.presets.contains_key(&name),
                "enabled": preset.enabled,
                "aggregator": slot(&preset.aggregator),
                "references": preset.reference_models.iter().map(slot).collect::<Vec<_>>(),
                "fanout": fanout(&preset.fanout),
                "reference_temperature": preset.reference_temperature,
                "aggregator_temperature": preset.aggregator_temperature,
                "billing_notice": pmt_providers::moa::billing_notice(config, &name),
            })
        })
        .collect();
    json!({ "default_preset": moa.default_preset, "presets": presets })
}

fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    if ctx.json {
        return document("moa list", &presets_json(config));
    }
    let mut text = String::new();
    for (name, preset) in config.moa.all() {
        let marker = if name == config.moa.default_preset {
            " (default)"
        } else {
            ""
        };
        let state = if preset.enabled {
            ""
        } else {
            " [disabled: aggregator alone]"
        };
        let _ = writeln!(text, "{name}{marker}{state}");
        let _ = writeln!(
            text,
            "  aggregator: {} (acts with the tools and is billed for the tool loop)",
            preset.aggregator.label()
        );
        let references: Vec<String> = preset.reference_models.iter().map(MoaSlot::label).collect();
        let _ = writeln!(
            text,
            "  references: {} (advise on {})",
            references.join(", "),
            fanout(&preset.fanout)
        );
        if let Some(notice) = pmt_providers::moa::billing_notice(config, &name) {
            let _ = writeln!(text, "  {notice}");
        }
    }
    print_raw(&text)
}

/// Deletes configured preset `name`.
///
/// # Errors
///
/// `moa.default_preset_protected` (exit 1) for the default preset;
/// `moa.preset_not_found` for a preset the config does not define.
pub fn delete(scope: &ProfileScope, name: &str) -> Result<(), Error> {
    let moa = &scope.config().moa;
    if name == moa.default_preset {
        return Err(Error::new(
            &codes::MOA_DEFAULT_PRESET_PROTECTED,
            format!("preset '{name}' is moa.default_preset; set another default first"),
        )
        .with_hint("prometheus config set moa.default_preset <name>"));
    }
    if !scope.profile().config_file().remove_moa_preset(name)? {
        return Err(not_found(name));
    }
    Ok(())
}

/// Writes preset `name`, refusing slots on unknown providers.
///
/// # Errors
///
/// `config.invalid` for a recursive, empty or out-of-range preset or an
/// unknown slot provider; a managed lock; `home.io`.
pub fn put(scope: &ProfileScope, name: &str, preset: &MoaPreset) -> Result<(), Error> {
    let mut proposed = scope.config().clone();
    proposed.moa.presets.insert(name.to_owned(), preset.clone());
    let (registry, _) = crate::plugins_host::providers::registry(scope, &proposed)?;
    pmt_providers::moa::validate(&proposed, &registry)?;
    scope.profile().config_file().put_moa_preset(name, preset)
}

fn not_found(name: &str) -> Error {
    Error::new(
        &codes::MOA_PRESET_NOT_FOUND,
        format!("no configured preset '{name}' (the built-in 'default' cannot be deleted)"),
    )
    .with_hint("prometheus moa list")
}
