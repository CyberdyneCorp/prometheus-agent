//! `[moa.presets.<name>]` edits of the format-preserving document (spec:
//! mixture-of-agents, Preset management commands): slots are written as
//! inline tables so a preset reads as one block.

use pmt_core::error::{Error, codes};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use crate::schema::{MoaFanout, MoaPreset, MoaSlot};

/// Sets preset `name` to `preset`, replacing any earlier definition.
pub(crate) fn set(doc: &mut DocumentMut, name: &str, preset: &MoaPreset) -> Result<(), Error> {
    let presets = presets(doc)?;
    presets.insert(name, Item::Table(table(preset)));
    Ok(())
}

/// Removes preset `name`; whether it was defined.
pub(crate) fn remove(doc: &mut DocumentMut, name: &str) -> bool {
    let Some(presets) = doc
        .get_mut("moa")
        .and_then(|moa| moa.get_mut("presets"))
        .and_then(Item::as_table_like_mut)
    else {
        return false;
    };
    presets.remove(name).is_some()
}

fn presets(doc: &mut DocumentMut) -> Result<&mut Table, Error> {
    let not_table =
        |key: &str| Error::new(&codes::CONFIG_INVALID, format!("[{key}] is not a table"));
    let moa = doc
        .entry("moa")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| not_table("moa"))?;
    let presets = moa.entry("presets").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    presets
        .as_table_mut()
        .ok_or_else(|| not_table("moa.presets"))
}

fn table(preset: &MoaPreset) -> Table {
    let mut table = Table::new();
    let mut references = Array::new();
    for slot in &preset.reference_models {
        references.push(slot_table(slot));
    }
    table.insert("reference_models", value(references));
    table.insert("aggregator", value(slot_table(&preset.aggregator)));
    if let Some(t) = preset.reference_temperature {
        table.insert("reference_temperature", value(t));
    }
    if let Some(t) = preset.aggregator_temperature {
        table.insert("aggregator_temperature", value(t));
    }
    if preset.fanout != MoaFanout::default() {
        table.insert("fanout", value(fanout(&preset.fanout)));
    }
    if !preset.enabled {
        table.insert("enabled", value(false));
    }
    table
}

fn slot_table(slot: &MoaSlot) -> InlineTable {
    let mut inline = InlineTable::new();
    inline.insert("provider", slot.provider.as_str().into());
    inline.insert("model", slot.model.as_str().into());
    if let Some(effort) = slot.reasoning_effort {
        inline.insert("reasoning_effort", effort.as_str().into());
    }
    inline
}

fn fanout(fanout: &MoaFanout) -> toml_edit::Value {
    match fanout {
        MoaFanout::Text(text) => text.as_str().into(),
        MoaFanout::Table { mode, n } => {
            let mut inline = InlineTable::new();
            inline.insert("mode", mode.as_str().into());
            if let Some(n) = n {
                inline.insert("n", (*n).into());
            }
            toml_edit::Value::InlineTable(inline)
        }
    }
}
