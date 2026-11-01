//! `scenario-index.toml`: the header keys, then one table per capability
//! with one sub-table per requirement whose keys are its scenarios:
//!
//! ```toml
//! specs = "openspec/changes/<change>/specs"
//!
//! [milestones.M1]
//! capabilities = ["cli"]
//!
//! [milestones.M2]
//! partial = true
//! capabilities = ["cli", "tui"]
//!
//! [cli."Top-level command tree"]
//! "Unknown subcommand" = ["crates/prometheus/tests/chat_cli.rs::a_mistyped_subcommand_exits_2_with_a_suggestion"]
//! "Default subcommand" = { tests = ["<file>::<fn>"], note = "why these tests prove it" }
//! "Later scenario" = { deferred = "M3: needs the dashboard (task 61.1)" }
//! ```
//!
//! A milestone marked `partial` is still being implemented: its scenarios
//! are indexed as their tasks land, so an unindexed one is pending rather
//! than a failure, while every entry it has is checked like any other.

use std::collections::BTreeMap;
use std::path::Path;

use toml::{Table, Value};

/// What an index entry says about a scenario.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// `<file>::<fn>` test references.
    Tests(Vec<String>),
    /// Not testable in this milestone, with the reason.
    Deferred(String),
}

/// One milestone whose scenarios are indexed.
#[derive(Debug)]
pub(crate) struct Milestone {
    /// Its name, as the specs' `**Milestone:**` lines spell it.
    pub name: String,
    /// The capabilities covered.
    pub capabilities: Vec<String>,
    /// Unindexed scenarios are pending, not failures.
    pub partial: bool,
}

/// The parsed index.
#[derive(Debug)]
pub(crate) struct Index {
    /// The specs directory, relative to the repository root.
    pub specs: String,
    /// The milestones whose scenarios are indexed, in name order.
    pub milestones: Vec<Milestone>,
    /// Entries by scenario key (`capability › requirement › scenario`).
    pub entries: BTreeMap<String, Entry>,
}

impl Index {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut table: Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let specs = take_string(&mut table, "specs")?;
        let milestones = match table.remove("milestones") {
            Some(Value::Table(milestones)) => milestones
                .iter()
                .map(|(name, value)| milestone(name, value))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("`milestones` must be a table of milestones".to_owned()),
        };
        let mut entries = BTreeMap::new();
        for (capability, requirements) in table {
            for (requirement, scenarios) in as_table(&requirements, &capability)? {
                for (scenario, value) in as_table(scenarios, requirement)? {
                    let key = super::spec::key(&capability, requirement, scenario);
                    entries.insert(key.clone(), entry(&key, value)?);
                }
            }
        }
        Ok(Self {
            specs,
            milestones,
            entries,
        })
    }

    /// The deferred entries and their reasons.
    pub(crate) fn deferred(&self) -> Vec<(&str, &str)> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| match entry {
                Entry::Deferred(reason) => Some((key.as_str(), reason.as_str())),
                Entry::Tests(_) => None,
            })
            .collect()
    }
}

fn milestone(name: &str, value: &Value) -> Result<Milestone, String> {
    let table = as_table(value, name)?;
    let capabilities = match table.get("capabilities") {
        Some(Value::Array(items)) => items.iter().map(string).collect::<Result<_, _>>()?,
        _ => return Err(format!("`milestones.{name}.capabilities` must be an array")),
    };
    let partial = match table.get("partial") {
        None => false,
        Some(Value::Boolean(partial)) => *partial,
        Some(_) => return Err(format!("`milestones.{name}.partial` must be a boolean")),
    };
    if let Some(unknown) = table
        .keys()
        .find(|key| !["capabilities", "partial"].contains(&key.as_str()))
    {
        return Err(format!("`milestones.{name}` has unknown key `{unknown}`"));
    }
    Ok(Milestone {
        name: name.to_owned(),
        capabilities,
        partial,
    })
}

fn entry(key: &str, value: &Value) -> Result<Entry, String> {
    match value {
        Value::Array(items) => Ok(Entry::Tests(
            items.iter().map(string).collect::<Result<_, _>>()?,
        )),
        Value::Table(table) => match (table.get("tests"), table.get("deferred")) {
            (Some(tests), None) => entry(key, tests),
            (None, Some(reason)) => Ok(Entry::Deferred(string(reason)?)),
            _ => Err(format!("{key}: expected `tests` or `deferred`")),
        },
        _ => Err(format!("{key}: expected a list of tests or a table")),
    }
}

fn as_table<'a>(value: &'a Value, name: &str) -> Result<&'a Table, String> {
    value
        .as_table()
        .ok_or_else(|| format!("`{name}` must be a table"))
}

fn take_string(table: &mut Table, key: &str) -> Result<String, String> {
    match table.remove(key) {
        Some(value) => string(&value),
        None => Err(format!("missing `{key}`")),
    }
}

fn string(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("expected a string, found {value}"))
}
