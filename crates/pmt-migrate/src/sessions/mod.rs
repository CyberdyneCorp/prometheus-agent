//! The `sessions` component (spec: hermes-migration, Sessions imported
//! with lineage; Imported usage carried as events; Session import is
//! streaming, verified and indexed).
//!
//! The Hermes `state.db` is copied into the staging directory and only
//! the copy is opened. Sessions are imported parents first, in
//! transactions of at most [`BATCH`] sessions, each keeping its Hermes id
//! as a legacy id; after each commit the stored message counts are checked
//! against the source, and the search indexes are rebuilt when they went
//! stale before success is reported.

mod convert;
mod read;

use std::collections::{BTreeMap, BTreeSet};

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::legacy::LegacyLink;
use pmt_store::portable::Portable;

use crate::component::Component;
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::source::{self, Snapshot};

pub use convert::COST_SOURCE;
pub use read::SUPPORTED;

/// The legacy-id source of Hermes sessions.
pub const LEGACY_SOURCE: &str = "hermes";

/// Sessions per transaction.
pub const BATCH: usize = 500;

/// The planned sessions component.
pub struct SessionsStep {
    entries: Vec<Entry>,
    plan: Option<Planned>,
}

struct Planned {
    db: read::HermesDb,
    sessions: Vec<read::HermesSession>,
    /// Entry index per Hermes id.
    index: BTreeMap<String, usize>,
    // Dropped after `db` (fields drop in order): the staged copy is
    // removed once the connection is closed.
    _snapshot: Snapshot,
}

impl Step for SessionsStep {
    fn component(&self) -> Component {
        Component::Sessions
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn commits_in_parts(&self) -> bool {
        true
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let Some(plan) = &self.plan else {
            return Ok(());
        };
        let links = dest.store.legacy_links(LEGACY_SOURCE)?;
        let mut ids: BTreeMap<String, SessionId> = links
            .iter()
            .map(|(legacy, (id, _))| (legacy.clone(), *id))
            .collect();
        let by_id: BTreeMap<&str, &read::HermesSession> =
            plan.sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let mut pending = Vec::new();
        for s in parents_first(&plan.sessions) {
            let entry = &mut self.entries[plan.index[&s.id]];
            match links.get(&s.id) {
                None => pending.push(s),
                Some((id, carried)) => grow(plan, s, *id, *carried, dest, entry)?,
            }
        }
        let total = pending.len();
        for (n, chunk) in pending.chunks(BATCH).enumerate() {
            let rest = &pending[n * BATCH..];
            if dest.host.interrupted() {
                settle_rest(
                    &mut self.entries,
                    &plan.index,
                    rest,
                    Disposition::Skipped,
                    "interrupted; a re-run continues from here",
                );
                return Err(Error::new(
                    &codes::MIGRATE_INTERRUPTED,
                    format!("interrupted after {} of {total} sessions", n * BATCH),
                ));
            }
            let expected = match import_batch(plan, chunk, &by_id, &mut ids, dest) {
                Ok(expected) => expected,
                Err(e) => {
                    // Earlier batches stay committed (and linked, so a
                    // re-run skips them); this one and the rest are not.
                    settle_rest(
                        &mut self.entries,
                        &plan.index,
                        rest,
                        Disposition::Failed,
                        e.message(),
                    );
                    return Err(e);
                }
            };
            for (hermes, (id, count)) in expected {
                let stored = dest.store.session(id)?.message_count;
                if stored != count {
                    let e = &mut self.entries[plan.index[&hermes]];
                    e.disposition = Disposition::Failed;
                    e.reason = format!("stored {stored} of {count} messages");
                }
            }
            let done = (n * BATCH + chunk.len()).min(total);
            dest.host
                .progress(&format!("sessions: {done}/{total} imported"));
        }
        if dest.store.indexes_stale()? {
            dest.store.repair_indexes()?;
        }
        Ok(())
    }
}

/// Marks the entries of `rest` (sessions not committed) with
/// `disposition`.
fn settle_rest(
    entries: &mut [Entry],
    index: &BTreeMap<String, usize>,
    rest: &[&read::HermesSession],
    disposition: Disposition,
    why: &str,
) {
    for s in rest {
        let e = &mut entries[index[&s.id]];
        e.disposition = disposition;
        why.clone_into(&mut e.reason);
    }
}

/// An already imported session: appends the messages Hermes added since
/// (spec: hermes-migration, Grown Hermes session appended) unless it was
/// continued in Prometheus, which keeps it as a conflict.
fn grow(
    plan: &Planned,
    s: &read::HermesSession,
    id: SessionId,
    carried: u64,
    dest: &Dest<'_>,
    entry: &mut Entry,
) -> Result<(), Error> {
    let count = plan.db.message_count(&s.id)?;
    if count <= carried {
        entry.disposition = Disposition::Skipped;
        "already imported".clone_into(&mut entry.reason);
        return Ok(());
    }
    let rows = plan.db.messages(&s.id)?;
    let split = usize::try_from(carried)
        .unwrap_or(usize::MAX)
        .min(rows.len());
    let imported = rows[..split]
        .iter()
        .filter_map(|m| convert::message(m, id))
        .count() as u64;
    let added: Vec<_> = rows[split..]
        .iter()
        .filter_map(|m| convert::message(m, id))
        .collect();
    if dest.store.session(id)?.message_count != imported {
        entry.disposition = Disposition::Conflict;
        entry.reason = format!(
            "continued in Prometheus; {} newer Hermes messages were not appended",
            rows.len() - split
        );
        return Ok(());
    }
    dest.store.append_linked(
        &added,
        &LegacyLink {
            source: LEGACY_SOURCE.to_owned(),
            legacy_id: s.id.clone(),
            session: id,
            messages: rows.len() as u64,
        },
    )?;
    entry.disposition = Disposition::Transformed;
    entry.reason = format!("appended {} new messages", added.len());
    Ok(())
}

/// Imports one batch; returns each session's id and expected message
/// count.
fn import_batch(
    plan: &Planned,
    chunk: &[&read::HermesSession],
    by_id: &BTreeMap<&str, &read::HermesSession>,
    ids: &mut BTreeMap<String, SessionId>,
    dest: &Dest<'_>,
) -> Result<BTreeMap<String, (SessionId, u64)>, Error> {
    let mut portables = Vec::new();
    let mut links = Vec::new();
    let mut expected = BTreeMap::new();
    for s in chunk {
        let id = SessionId::new();
        let parent = s
            .parent_session_id
            .as_deref()
            .and_then(|p| Some((*ids.get(p)?, *by_id.get(p)?)))
            .map(|(pid, parent)| (pid, convert::lineage_of(s, parent)));
        let session = convert::session(s, id, parent);
        let rows = plan.db.messages(&s.id)?;
        let messages: Vec<_> = rows
            .iter()
            .filter_map(|m| convert::message(m, id))
            .collect();
        let usage = convert::usage(s, &plan.db.model_usage(&s.id)?, id);
        let count = messages.len() as u64;
        links.push(LegacyLink {
            source: LEGACY_SOURCE.to_owned(),
            legacy_id: s.id.clone(),
            session: id,
            messages: rows.len() as u64,
        });
        expected.insert(s.id.clone(), (id, count));
        ids.insert(s.id.clone(), id);
        portables.push(Portable {
            session,
            messages,
            usage,
            blob_refs: Vec::new(),
            session_blobs: Vec::new(),
        });
    }
    dest.store.import_linked(&portables, &links)?;
    Ok(expected)
}

/// `sessions` with every parent before its children (oldest first
/// otherwise); a parent missing from the list does not hold a child back.
fn parents_first(sessions: &[read::HermesSession]) -> Vec<&read::HermesSession> {
    let known: BTreeSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut order = Vec::with_capacity(sessions.len());
    let mut rest: Vec<&read::HermesSession> = sessions.iter().collect();
    while !rest.is_empty() {
        let before = rest.len();
        rest.retain(|s| {
            let ready = s
                .parent_session_id
                .as_deref()
                .is_none_or(|p| !known.contains(p) || placed.contains(p) || p == s.id);
            if ready {
                placed.insert(&s.id);
                order.push(*s);
            }
            !ready
        });
        if rest.len() == before {
            // A parent cycle: place the rest as they are.
            order.append(&mut rest);
        }
    }
    order
}

/// Plans the sessions component: stages the database copy and lists the
/// sessions.
///
/// # Errors
///
/// `migrate.component` for an unsupported schema or an unreadable copy.
pub fn plan(src: &Source<'_>) -> Result<SessionsStep, Error> {
    let mut step = SessionsStep {
        entries: Vec::new(),
        plan: None,
    };
    let Some(snapshot) = source::snapshot(src.profile, src.staging)? else {
        return Ok(step);
    };
    let db = read::HermesDb::open(&snapshot.database())?;
    let sessions = db.sessions()?;
    let mut index = BTreeMap::new();
    for s in &sessions {
        let count = db.message_count(&s.id)?;
        index.insert(s.id.clone(), step.entries.len());
        step.entries.push(
            Entry::new(Component::Sessions, &s.id, Disposition::Mapped)
                .to("session")
                .because(format!("{count} messages")),
        );
    }
    step.plan = Some(Planned {
        db,
        sessions,
        index,
        _snapshot: snapshot,
    });
    Ok(step)
}
