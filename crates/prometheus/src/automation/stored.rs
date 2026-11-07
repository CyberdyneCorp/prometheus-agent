//! A session's automation as its store rows hold it (one row per kind).

use std::collections::BTreeMap;

use pmt_automation::{Automation, Kind};
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::Store;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The stored automation of one session and who set each kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stored {
    /// What is set.
    pub automation: Automation,
    /// The surface that set each kind.
    pub owners: BTreeMap<Kind, String>,
}

impl Stored {
    /// The surface that set `kind` (`cli` when unknown).
    #[must_use]
    pub fn owner(&self, kind: Kind) -> &str {
        self.owners.get(&kind).map_or("cli", String::as_str)
    }
}

fn decode<T: DeserializeOwned>(row: &pmt_store::AutomationRow) -> Option<T> {
    serde_json::from_value(row.state.clone())
        .inspect_err(|err| {
            tracing::warn!(kind = row.kind, error = %err, "unreadable automation row skipped");
        })
        .ok()
}

/// The automation of `id`.
///
/// # Errors
///
/// Store errors.
pub fn load(store: &Store, id: SessionId) -> Result<Stored, Error> {
    let mut stored = Stored::default();
    for row in store.automations_of(id)? {
        let Some(kind) = Kind::parse(&row.kind) else {
            continue;
        };
        match kind {
            Kind::Goal => stored.automation.goal = decode(&row),
            Kind::Loop => stored.automation.looping = decode(&row),
            Kind::Heartbeat => stored.automation.heartbeat = decode(&row),
        }
        stored.owners.insert(kind, row.owner_surface);
    }
    Ok(stored)
}

fn put<T: Serialize + PartialEq>(
    store: &Store,
    id: SessionId,
    kind: Kind,
    (before, after): (Option<&T>, Option<&T>),
    owner: &str,
) -> Result<(), Error> {
    match after {
        Some(value) => {
            let state = serde_json::to_value(value).unwrap_or_default();
            store.put_automation(id, kind.as_str(), &state, owner)
        }
        None if before.is_some() => store.delete_automation(id, kind.as_str()).map(|_| ()),
        None => Ok(()),
    }
}

/// Writes the kinds that differ between `before` and `after`.
///
/// # Errors
///
/// Store errors.
pub fn save(store: &Store, id: SessionId, before: &Stored, after: &Stored) -> Result<(), Error> {
    let (b, a) = (&before.automation, &after.automation);
    let owner_changed = |kind| before.owner(kind) != after.owner(kind);
    if b.goal != a.goal || owner_changed(Kind::Goal) {
        let pair = (b.goal.as_ref(), a.goal.as_ref());
        put(store, id, Kind::Goal, pair, after.owner(Kind::Goal))?;
    }
    if b.looping != a.looping || owner_changed(Kind::Loop) {
        let pair = (b.looping.as_ref(), a.looping.as_ref());
        put(store, id, Kind::Loop, pair, after.owner(Kind::Loop))?;
    }
    if b.heartbeat != a.heartbeat || owner_changed(Kind::Heartbeat) {
        let pair = (b.heartbeat.as_ref(), a.heartbeat.as_ref());
        put(
            store,
            id,
            Kind::Heartbeat,
            pair,
            after.owner(Kind::Heartbeat),
        )?;
    }
    Ok(())
}
