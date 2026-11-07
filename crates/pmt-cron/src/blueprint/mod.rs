//! Parameterised automation blueprints (spec: scheduled-jobs, Automation
//! blueprint catalog): a key, a title, a `schedule_template` and a
//! `prompt_template` with `{slot}` placeholders, and typed slots. The
//! catalog is the built-ins ([`builtin`]), then enabled plugins' (keys
//! `<plugin>:<key>`) and skills' (`skill:<key>`) blueprints, never
//! replacing a built-in. A name resolves by exact key, then unique prefix,
//! then nearest spelling; [`Blueprint::fill`] checks the given values and
//! either names the next slot to ask for or renders the job's schedule and
//! prompt, so nobody types cron syntax.
//!
//! In a schedule template `{slot}` is the slot's schedule form (a cron day
//! field for `weekdays`, `H:MM` for `time`; a cron template is the five
//! fields, e.g. `{time.minute} {time.hour} * * {days}`) and `{time.hour}` /
//! `{time.minute}` the parts of a time; in a prompt template `{slot}` reads
//! as text.

pub mod builtin;
pub mod slots;

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

pub use self::slots::{Slot, SlotKind, Value};

/// Where a blueprint comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// The built-in catalog.
    Builtin,
    /// An enabled plugin.
    Plugin {
        /// Its id.
        id: String,
    },
    /// An installed skill's `blueprint` block.
    Skill {
        /// Its name.
        name: String,
    },
}

/// One blueprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blueprint {
    /// Its key (`<plugin>:<key>`, `skill:<key>` outside the built-ins).
    pub key: String,
    /// Its title (the job's default name).
    pub title: String,
    /// What it does.
    #[serde(default)]
    pub description: String,
    /// Its category (`daily`, `monitor`, `review`...).
    #[serde(default = "custom")]
    pub category: String,
    /// The schedule, with `{slot}` placeholders.
    pub schedule_template: String,
    /// The prompt, with `{slot}` placeholders.
    pub prompt_template: String,
    /// Its slots, asked in this order.
    #[serde(default)]
    pub slots: Vec<Slot>,
    /// Where it comes from.
    #[serde(default = "builtin_origin")]
    pub origin: Origin,
}

fn custom() -> String {
    "custom".to_owned()
}

fn builtin_origin() -> Origin {
    Origin::Builtin
}

/// `cron.blueprint` (65).
fn refused(message: impl Into<String>) -> Error {
    Error::new(&codes::CRON_BLUEPRINT, message)
}

/// What filling a blueprint gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fill {
    /// This slot has no value yet: ask for it.
    Missing(Slot),
    /// Every slot has a value.
    Ready(Filled),
}

/// A filled blueprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    /// The blueprint's key.
    pub key: String,
    /// The job's name.
    pub title: String,
    /// The schedule, in the grammar of `prometheus cron`.
    pub schedule: String,
    /// The prompt.
    pub prompt: String,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The `{...}` placeholders of `template`.
fn placeholders(template: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        found.push(&after[..end]);
        rest = &after[end + 1..];
    }
    found
}

impl Blueprint {
    /// A blueprint declared by a plugin or a skill as data (a manifest
    /// table, a `blueprint` block), its key namespaced with `namespace`
    /// (`<plugin>` or `skill`); `key` defaults to `default_key`.
    ///
    /// # Errors
    ///
    /// `cron.blueprint` naming what is wrong.
    pub fn declared(
        value: serde_json::Value,
        namespace: &str,
        default_key: &str,
        origin: Origin,
    ) -> Result<Self, Error> {
        let mut value = value;
        if let Some(map) = value.as_object_mut() {
            map.entry("key")
                .or_insert_with(|| serde_json::Value::String(default_key.to_owned()));
            // `schedule` / `prompt` read as the templates.
            for (short, long) in [
                ("schedule", "schedule_template"),
                ("prompt", "prompt_template"),
            ] {
                if let Some(v) = map.remove(short) {
                    map.entry(long).or_insert(v);
                }
            }
            map.remove("origin");
        }
        let mut blueprint: Self = serde_json::from_value(value)
            .map_err(|e| refused(format!("blueprint of {namespace}: {e}")))?;
        if !valid_key(&blueprint.key) {
            return Err(refused(format!(
                "blueprint key '{}' of {namespace}: use a-z, 0-9, '-' and '_'",
                blueprint.key
            )));
        }
        blueprint.key = format!("{namespace}:{}", blueprint.key);
        blueprint.origin = origin;
        blueprint.check()?;
        Ok(blueprint)
    }

    /// Checks that every placeholder names a slot (and only a time has
    /// parts) and that slot names are unique.
    ///
    /// # Errors
    ///
    /// `cron.blueprint` naming the placeholder or slot.
    pub fn check(&self) -> Result<(), Error> {
        for (at, slot) in self.slots.iter().enumerate() {
            if self.slots[..at].iter().any(|s| s.name == slot.name) {
                return Err(refused(format!(
                    "blueprint '{}': slot '{}' is declared twice",
                    self.key, slot.name
                )));
            }
            if slot.kind == SlotKind::Enum && slot.options.is_empty() {
                return Err(refused(format!(
                    "blueprint '{}': enum slot '{}' has no options",
                    self.key, slot.name
                )));
            }
        }
        let templates = [&self.schedule_template, &self.prompt_template];
        for placeholder in templates.iter().flat_map(|t| placeholders(t)) {
            let (name, part) = placeholder.split_once('.').unwrap_or((placeholder, ""));
            let slot = self.slots.iter().find(|s| s.name == name);
            let fits = slot.is_some_and(|s| {
                part.is_empty() || (s.kind == SlotKind::Time && matches!(part, "hour" | "minute"))
            });
            if !fits {
                return Err(refused(format!(
                    "blueprint '{}': placeholder {{{placeholder}}} names no slot",
                    self.key
                )));
            }
        }
        Ok(())
    }

    /// The key without its namespace.
    #[must_use]
    pub fn short_key(&self) -> &str {
        self.key.rsplit(':').next().unwrap_or(&self.key)
    }

    /// Checks `values` (slot name -> text); the first slot still to ask
    /// for, or the rendered schedule and prompt.
    ///
    /// # Errors
    ///
    /// `cron.blueprint`: `unknown slot '<name>'`, `invalid value for slot
    /// '<name>': <what it expects>`.
    pub fn fill(&self, values: &BTreeMap<String, String>) -> Result<Fill, Error> {
        if let Some(unknown) = values
            .keys()
            .find(|k| !self.slots.iter().any(|s| &s.name == *k))
        {
            return Err(refused(format!(
                "unknown slot '{unknown}' for blueprint '{}' (slots: {})",
                self.key,
                self.slots
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let mut checked = BTreeMap::new();
        for slot in &self.slots {
            let given = values.get(&slot.name).or(slot.default.as_ref());
            let value = match given {
                Some(text) => slot.check(text).map_err(|why| {
                    refused(format!("invalid value for slot '{}': {why}", slot.name))
                })?,
                None if slot.required => return Ok(Fill::Missing(slot.clone())),
                None => Value {
                    display: String::new(),
                    fields: BTreeMap::from([("", String::new())]),
                },
            };
            checked.insert(slot.name.as_str(), value);
        }
        Ok(Fill::Ready(Filled {
            key: self.key.clone(),
            title: self.title.clone(),
            schedule: render(&self.schedule_template, &checked, true),
            prompt: render(&self.prompt_template, &checked, false),
        }))
    }
}

/// `template` with its placeholders replaced by schedule or prompt forms.
fn render(template: &str, values: &BTreeMap<&str, Value>, schedule: bool) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let placeholder = &after[..end];
        let (name, part) = placeholder.split_once('.').unwrap_or((placeholder, ""));
        let text = values.get(name).map_or("", |v| {
            if schedule || !part.is_empty() {
                v.fields.get(part).map_or("", String::as_str)
            } else {
                v.display.as_str()
            }
        });
        out.push_str(text);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The edit distance of `a` and `b` (for the nearest spelling).
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[b.len()]
}

/// The blueprint `name` means: its key (or the key without namespace),
/// else the one key it starts, else the nearest spelling.
///
/// # Errors
///
/// `cron.blueprint`: an ambiguous prefix (listing the keys) or no
/// blueprint near the name.
pub fn resolve<'a>(catalog: &'a [Blueprint], name: &str) -> Result<&'a Blueprint, Error> {
    let name = name.trim().to_ascii_lowercase();
    if let Some(exact) = catalog.iter().find(|b| b.key == name) {
        return Ok(exact);
    }
    let short: Vec<&Blueprint> = catalog.iter().filter(|b| b.short_key() == name).collect();
    if let [only] = short.as_slice() {
        return Ok(only);
    }
    let prefixed: Vec<&Blueprint> = catalog
        .iter()
        .filter(|b| b.key.starts_with(&name) || b.short_key().starts_with(&name))
        .collect();
    match prefixed.as_slice() {
        [only] => return Ok(only),
        [] => {}
        many => {
            let keys: Vec<&str> = many.iter().map(|b| b.key.as_str()).collect();
            return Err(refused(format!(
                "blueprint '{name}' is ambiguous: {}",
                keys.join(", ")
            )));
        }
    }
    let nearest = catalog
        .iter()
        .map(|b| (distance(&name, b.short_key()), b))
        .min_by_key(|(d, _)| *d)
        .filter(|(d, _)| *d <= 3);
    nearest.map(|(_, b)| b).ok_or_else(|| {
        refused(format!(
            "unknown blueprint '{name}' (`/blueprint` lists them)"
        ))
    })
}

/// `name=value` words as slot values; other words are refused.
///
/// # Errors
///
/// `cron.blueprint` naming a word that is not `slot=value`.
pub fn values(words: &[String]) -> Result<BTreeMap<String, String>, Error> {
    let mut values = BTreeMap::new();
    for word in words {
        let Some((name, value)) = word.split_once('=') else {
            return Err(refused(format!(
                "'{word}' is not slot=value (e.g. time=08:00)"
            )));
        };
        values.insert(name.trim().to_owned(), value.trim().to_owned());
    }
    Ok(values)
}

#[cfg(test)]
mod tests;
