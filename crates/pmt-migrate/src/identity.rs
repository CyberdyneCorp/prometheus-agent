//! The `identity` component (spec: hermes-migration, Memory and identity
//! imported; Config translation from user-set values only): `SOUL.md` is
//! the profile identity file, and the Hermes personality selection (or
//! its user-owned `agent.system_prompt`) becomes a section of it, never a
//! config key.

use pmt_config::profiles::IDENTITY_FILE;
use pmt_core::error::{Error, codes};
use serde_json::Value;

use crate::component::Component;
use crate::config::flatten;
use crate::data;
use crate::ledger::{self, Decision};
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes;

/// Names that select no personality.
const NEUTRAL: &[&str] = &["", "none", "default", "neutral"];

/// The planned identity component.
pub struct IdentityStep {
    entries: Vec<Entry>,
    soul: Option<String>,
    soul_entries: Vec<usize>,
}

impl Step for IdentityStep {
    fn component(&self) -> Component {
        Component::Identity
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let Some(soul) = &self.soul else {
            return Ok(());
        };
        let path = dest.scope.dir().join(IDENTITY_FILE);
        let current = std::fs::read_to_string(&path)
            .ok()
            .filter(|old| !old.trim().is_empty())
            .map(|old| ledger::digest(old.as_bytes()));
        let new = ledger::digest(soul.as_bytes());
        let key = format!("file:{IDENTITY_FILE}");
        let prior = dest.ledger.borrow().get(&key).cloned();
        let decision = ledger::decide(
            prior.as_ref(),
            &new,
            &new,
            current.as_deref(),
            dest.overwrite,
        );
        let why = format!("{IDENTITY_FILE} already has other content");
        for i in &self.soul_entries {
            writes::settle(decision, &mut self.entries[*i], &why);
        }
        if decision == Decision::Write {
            pmt_core::private_fs::write_atomic(&path, soul.as_bytes())
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", &path, &e))?;
        }
        if matches!(decision, Decision::Write | Decision::Same) {
            dest.ledger.borrow_mut().record(key, new.clone(), new);
        }
        Ok(())
    }
}

/// Plans the identity component.
///
/// # Errors
///
/// `migrate.component` when `SOUL.md` exists but cannot be read.
pub fn plan(src: &Source<'_>) -> Result<IdentityStep, Error> {
    let mut step = IdentityStep {
        entries: Vec::new(),
        soul: None,
        soul_entries: Vec::new(),
    };
    let mut parts = Vec::new();
    if let Some(text) = src.profile.read("SOUL.md")?
        && !text.trim().is_empty()
    {
        step.soul_entries.push(step.entries.len());
        step.entries.push(
            Entry::new(Component::Identity, "SOUL.md", Disposition::Mapped).to(IDENTITY_FILE),
        );
        parts.push(text.trim_end().to_owned());
    }
    if let Some(overlay) = overlay(src, &mut step) {
        parts.push(overlay);
    }
    if !parts.is_empty() {
        step.soul = Some(parts.join("\n\n") + "\n");
    }
    Ok(step)
}

/// The selected personality's section, reporting every identity key.
fn overlay(src: &Source<'_>, step: &mut IdentityStep) -> Option<String> {
    let selected = ["display.personality", "agent.personality"]
        .iter()
        .find_map(|k| src.get(k).and_then(Value::as_str).map(|v| (*k, v)))
        .map(|(k, v)| (k, v.trim().to_lowercase()))
        .filter(|(_, v)| !NEUTRAL.contains(&v.as_str()));
    let mut section = None;
    if let Some((key, name)) = &selected {
        match personality_text(src, name) {
            Some((text, from)) => {
                section = Some(format!("## Personality: {name}\n\n{}", text.trim()));
                step.soul_entries.push(step.entries.len());
                step.entries.push(
                    Entry::new(Component::Identity, *key, Disposition::Transformed)
                        .to(IDENTITY_FILE)
                        .showing(name)
                        .because(format!(
                            "the {from} personality's text added to {IDENTITY_FILE}"
                        )),
                );
            }
            None => step.entries.push(
                Entry::new(Component::Identity, *key, Disposition::Unsupported)
                    .showing(name)
                    .because("no such personality in Hermes"),
            ),
        }
    }
    let system_prompt = src
        .get("agent.system_prompt")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty());
    if let Some(text) = system_prompt {
        if section.is_none() {
            section = Some(format!("## Instructions\n\n{}", text.trim()));
            step.soul_entries.push(step.entries.len());
            step.entries.push(
                Entry::new(
                    Component::Identity,
                    "agent.system_prompt",
                    Disposition::Transformed,
                )
                .to(IDENTITY_FILE)
                .because(format!("added to {IDENTITY_FILE}")),
            );
        } else {
            step.entries.push(
                Entry::new(
                    Component::Identity,
                    "agent.system_prompt",
                    Disposition::Skipped,
                )
                .because("Hermes uses it only without a personality"),
            );
        }
    }
    report_rest(src, selected.as_ref().map(|(k, n)| (*k, n.as_str())), step);
    section
}

/// Entries for the keys the selection did not consume.
fn report_rest(src: &Source<'_>, selected: Option<(&str, &str)>, step: &mut IdentityStep) {
    for key in ["display.personality", "agent.personality"] {
        let chosen = selected.is_some_and(|(k, _)| k == key);
        if let Some(value) = src.get(key).filter(|_| !chosen) {
            let neutral = value.as_str().is_some_and(|v| NEUTRAL.contains(&v.trim()));
            let (d, why) = if neutral {
                (Disposition::DefaultOmitted, "no personality")
            } else {
                (Disposition::Skipped, "another key selects the personality")
            };
            step.entries
                .push(Entry::new(Component::Identity, key, d).because(why));
        }
    }
    for root in ["personalities", "agent.personalities"] {
        let defs = src.get(root).and_then(Value::as_object);
        for (name, def) in defs.into_iter().flatten() {
            let used = selected.is_some_and(|(_, n)| n == name.to_lowercase());
            report_definition(&format!("{root}.{name}"), def, used, step);
        }
    }
}

/// Entries for every key of one personality definition.
fn report_definition(root: &str, def: &Value, used: bool, step: &mut IdentityStep) {
    let mut leaves = Vec::new();
    flatten(def, root, &mut leaves);
    for (key, _) in leaves {
        let e = if used {
            Entry::new(Component::Identity, key, Disposition::Transformed)
                .to(IDENTITY_FILE)
                .because("the selected personality")
        } else {
            Entry::new(Component::Identity, key, Disposition::Skipped)
                .because("not the selected personality")
        };
        step.entries.push(e);
    }
}

/// The text of personality `name` (user definitions win over built-ins)
/// and where it came from.
fn personality_text(src: &Source<'_>, name: &str) -> Option<(String, &'static str)> {
    for root in ["agent.personalities", "personalities"] {
        let defs = src.get(root).and_then(Value::as_object);
        let def = defs.and_then(|d| d.iter().find(|(k, _)| k.to_lowercase() == name));
        if let Some((_, def)) = def {
            return Some((render(def), "custom"));
        }
    }
    data::builtin_personality(name).map(|t| (t.to_owned(), "built-in"))
}

/// Hermes' rendering: a string, a list of lines, or `system_prompt` with
/// optional `tone` and `style`.
fn render(def: &Value) -> String {
    match def {
        Value::Object(map) => {
            let mut parts = vec![map.get("system_prompt").map(text).unwrap_or_default()];
            if let Some(tone) = map.get("tone") {
                parts.push(format!("Tone: {}", text(tone)));
            }
            if let Some(style) = map.get("style") {
                parts.push(format!("Style: {}", text(style)));
            }
            parts.retain(|p| !p.trim().is_empty());
            parts.join("\n")
        }
        other => text(other),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_owned(),
        Value::Array(items) => items
            .iter()
            .map(text)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
