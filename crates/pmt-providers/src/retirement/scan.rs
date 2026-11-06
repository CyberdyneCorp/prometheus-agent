//! Finding retired and deprecated model ids in a profile's config (spec:
//! configuration-and-profiles, Retired model rewrite).
//!
//! Every key that holds a model id is read the way its consumer reads it:
//! `<provider>/<model>` with a registered provider name, else a bare id of
//! the key's provider (its sibling `provider` key, else `model.provider`).
//! A replacement is written back in the same form: a bare id stays bare
//! when the provider is unchanged, otherwise the provider key changes too
//! (or the value gains the `<provider>/` prefix).

use pmt_config::Config;
use pmt_config::schema::{FallbackEntry, ModelAlias};
use serde::Serialize;

use super::{Retirement, Status, lookup};
use crate::registry::ProviderRegistry;

/// One config key naming a deprecated or retired model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Dotted key (`model.default`, `fallback.chain[0].model`).
    pub key: String,
    /// The value as written.
    pub from: String,
    /// The value it is rewritten to (`None`: no replacement is known).
    pub to: Option<String>,
    /// Deprecated or retired.
    pub status: Status,
    /// The table entry's `<provider>/<model>`.
    pub id: String,
}

/// What `config fix-models --apply` writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewrite {
    /// Every finding, in key order.
    pub findings: Vec<Finding>,
    /// `(key, value)` pairs for `ConfigFile::set_all`.
    pub sets: Vec<(String, String)>,
    /// The rewritten `[[fallback.chain]]`, when an entry changed.
    pub chain: Option<Vec<FallbackEntry>>,
}

/// A model-holding slot: its key, value and the provider a bare id
/// belongs to.
struct Slot<'a> {
    key: String,
    value: &'a str,
    /// The sibling provider key, when the slot has one.
    provider_key: Option<String>,
    provider: Option<&'a str>,
}

/// A replacement as the slot writes it.
struct Placed {
    value: String,
    provider: Option<String>,
}

/// Scans every key of `config` that holds a model id.
#[must_use]
pub fn scan(config: &Config, registry: &ProviderRegistry) -> Rewrite {
    let main = config.model.provider.as_deref();
    let mut rewrite = Rewrite::default();
    for slot in slots(config, main) {
        let Some((entry, placed)) = examine(config, registry, &slot) else {
            continue;
        };
        if let Some(placed) = &placed {
            rewrite.sets.push((slot.key.clone(), placed.value.clone()));
            if let (Some(key), Some(provider)) = (&slot.provider_key, &placed.provider) {
                rewrite.sets.push((key.clone(), provider.clone()));
            }
        }
        rewrite
            .findings
            .push(finding(&slot, &entry, placed.as_ref()));
    }
    chain(config, registry, &mut rewrite);
    rewrite
}

fn finding(slot: &Slot<'_>, entry: &Retirement, placed: Option<&Placed>) -> Finding {
    Finding {
        key: slot.key.clone(),
        from: slot.value.to_owned(),
        to: placed.map(|p| p.value.clone()),
        status: entry.status,
        id: entry.id.clone(),
    }
}

fn slots<'a>(config: &'a Config, main: Option<&'a str>) -> Vec<Slot<'a>> {
    let mut slots = Vec::new();
    if let Some(value) = config.model.default.as_deref() {
        slots.push(Slot {
            key: "model.default".to_owned(),
            value,
            provider_key: Some("model.provider".to_owned()),
            provider: main,
        });
    }
    for (name, alias) in &config.model.aliases {
        let (value, provider_key, provider) = match alias {
            ModelAlias::Ref(reference) => (reference.as_str(), None, None),
            ModelAlias::Route(route) => (
                route.model.as_str(),
                Some(format!("model.aliases.{name}.provider")),
                Some(route.provider.as_str()),
            ),
        };
        let key = match alias {
            ModelAlias::Ref(_) => format!("model.aliases.{name}"),
            ModelAlias::Route(_) => format!("model.aliases.{name}.model"),
        };
        slots.push(Slot {
            key,
            value,
            provider_key,
            provider,
        });
    }
    auxiliary_slots(config, main, &mut slots);
    let delegation = &config.delegation;
    if let Some(value) = delegation.model.as_deref() {
        slots.push(Slot {
            key: "delegation.model".to_owned(),
            value,
            provider_key: None,
            provider: main,
        });
    }
    if let Some(value) = delegation.review.model.as_deref() {
        slots.push(Slot {
            key: "delegation.review.model".to_owned(),
            value,
            provider_key: Some("delegation.review.provider".to_owned()),
            provider: delegation.review.provider.as_deref().or(main),
        });
    }
    slots
}

/// `auxiliary.<task>.model`, read through the typed section's fields.
fn auxiliary_slots<'a>(config: &'a Config, main: Option<&'a str>, slots: &mut Vec<Slot<'a>>) {
    let aux = &config.auxiliary;
    let tasks = [
        ("compression", &aux.compression),
        ("title", &aux.title),
        ("vision", &aux.vision),
        ("approval", &aux.approval),
        ("memory", &aux.memory),
        ("curator", &aux.curator),
        ("web_extract", &aux.web_extract),
        ("skill_review", &aux.skill_review),
        ("evolution", &aux.evolution),
        ("goal_judge", &aux.goal_judge),
        ("side_question", &aux.side_question),
        ("voice_chat", &aux.voice_chat),
    ];
    for (name, task) in tasks {
        let Some(task) = task else { continue };
        let Some(value) = task.model.as_deref() else {
            continue;
        };
        slots.push(Slot {
            key: format!("auxiliary.{name}.model"),
            value,
            provider_key: Some(format!("auxiliary.{name}.provider")),
            provider: task.provider.as_deref().filter(|p| *p != "auto").or(main),
        });
    }
}

/// The retirement entry of `slot`'s model and, when it has a
/// replacement, how the slot writes it.
fn examine(
    config: &Config,
    registry: &ProviderRegistry,
    slot: &Slot<'_>,
) -> Option<(Retirement, Option<Placed>)> {
    let (provider, model, prefixed) = interpret(registry, slot.value, slot.provider)?;
    let entry = lookup(config, &provider, &model)?;
    let placed = entry.replacement.as_deref().and_then(|replacement| {
        let (to_provider, to_model) = replacement.split_once('/')?;
        Some(place(slot, &provider, prefixed, to_provider, to_model))
    });
    Some((entry, placed))
}

fn place(
    slot: &Slot<'_>,
    provider: &str,
    prefixed: bool,
    to_provider: &str,
    to_model: &str,
) -> Placed {
    let same = to_provider == provider;
    match (prefixed, same, &slot.provider_key) {
        (false, true, _) => Placed {
            value: to_model.to_owned(),
            provider: None,
        },
        (false, false, Some(_)) => Placed {
            value: to_model.to_owned(),
            provider: Some(to_provider.to_owned()),
        },
        _ => Placed {
            value: format!("{to_provider}/{to_model}"),
            provider: None,
        },
    }
}

/// `(canonical provider, model, written with a provider prefix)`.
pub(super) fn interpret(
    registry: &ProviderRegistry,
    value: &str,
    provider: Option<&str>,
) -> Option<(String, String, bool)> {
    if let Some((prefix, model)) = value.split_once('/')
        && let Some(id) = registry.canonical(prefix)
    {
        return Some((id.to_owned(), model.to_owned(), true));
    }
    let provider = provider?;
    let id = registry.canonical(provider).unwrap_or(provider);
    Some((id.to_owned(), value.to_owned(), false))
}

fn chain(config: &Config, registry: &ProviderRegistry, rewrite: &mut Rewrite) {
    let mut entries = config.fallback.chain.clone();
    let mut changed = false;
    for (i, entry) in config.fallback.chain.iter().enumerate() {
        let slot = Slot {
            key: format!("fallback.chain[{i}].model"),
            value: &entry.model,
            provider_key: Some(format!("fallback.chain[{i}].provider")),
            provider: Some(&entry.provider),
        };
        let Some((found, placed)) = examine(config, registry, &slot) else {
            continue;
        };
        if let Some(placed) = &placed {
            entries[i].model.clone_from(&placed.value);
            if let Some(provider) = &placed.provider {
                entries[i].provider.clone_from(provider);
            }
            changed = true;
        }
        rewrite
            .findings
            .push(finding(&slot, &found, placed.as_ref()));
    }
    if changed {
        rewrite.chain = Some(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(text: &str) -> Rewrite {
        let config: Config = toml::from_str(text).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        scan(&config, &registry)
    }

    #[test]
    fn a_bare_default_is_rewritten_bare_and_a_prefixed_one_prefixed() {
        let bare = scanned("[model]\nprovider = \"xai\"\ndefault = \"grok-2\"\n");
        assert_eq!(bare.findings[0].key, "model.default");
        assert_eq!(bare.findings[0].to.as_deref(), Some("grok-4"));
        assert_eq!(
            bare.sets,
            [("model.default".to_owned(), "grok-4".to_owned())]
        );
        let prefixed = scanned("[model]\ndefault = \"xai/grok-2\"\n");
        assert_eq!(prefixed.findings[0].to.as_deref(), Some("xai/grok-4"));
    }

    #[test]
    fn auxiliary_delegation_aliases_and_chain_entries_are_scanned() {
        let rewrite = scanned(
            "[model]\nprovider = \"anthropic\"\ndefault = \"claude-sonnet-4-5\"\n\
             [model.aliases]\nold = \"xai/grok-beta\"\n\
             [auxiliary.title]\nprovider = \"xai\"\nmodel = \"grok-3-mini\"\n\
             [delegation]\nmodel = \"claude-3-opus-20240229\"\n\
             [[fallback.chain]]\nprovider = \"grok\"\nmodel = \"grok-2\"\n",
        );
        let keys: Vec<(&str, Option<&str>)> = rewrite
            .findings
            .iter()
            .map(|f| (f.key.as_str(), f.to.as_deref()))
            .collect();
        assert_eq!(
            keys,
            [
                ("model.aliases.old", Some("xai/grok-4")),
                ("auxiliary.title.model", Some("grok-4-fast")),
                ("delegation.model", Some("claude-opus-4-1")),
                ("fallback.chain[0].model", Some("grok-4")),
            ]
        );
        assert_eq!(rewrite.findings[1].status, Status::Deprecated);
        assert_eq!(rewrite.chain.as_ref().unwrap()[0].model, "grok-4");
    }

    #[test]
    fn a_replacement_on_another_provider_moves_the_provider_key() {
        let rewrite = scanned(
            "[model]\nprovider = \"xai\"\ndefault = \"grok-2\"\n\
             [models.\"xai/grok-2\"]\nreplacement = \"openrouter/x-ai/grok-4\"\n",
        );
        assert_eq!(
            rewrite.sets,
            [
                ("model.default".to_owned(), "x-ai/grok-4".to_owned()),
                ("model.provider".to_owned(), "openrouter".to_owned()),
            ]
        );
    }
}
