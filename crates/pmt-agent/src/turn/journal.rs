//! The deterministic turn journal (spec: agent-turn-loop, Deterministic
//! turn journal).
//!
//! A turn writes `turn_started`, then per model request `request_sent`
//! (the canonical request hash) and `response_received`, per tool call
//! `tool_started` (with the tool's declared effect and idempotence) and
//! `tool_finished` (the result's blob ref), and finally `turn_finished`.
//! Each record is committed before the side effect it announces begins,
//! so after a crash the newest record names the exact point reached
//! ([`super::resume`] picks the turn up from there).

use std::collections::HashMap;
use std::sync::Arc;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_providers::request::Request;
use pmt_store::journal::BLOB_OWNER;
use pmt_store::{JournalKind, JournalRecord, Store};
use pmt_tools::ToolResult;

use super::model::RequestedCall;
use super::run::Turn;
use super::tools::{CallJournal, Declared};

/// The canonical hash of a request: its model, system blocks, tools and
/// messages in JSON form.
fn request_hash(request: &Request) -> String {
    let canonical = (
        &request.model,
        &request.system,
        &request.tools,
        &request.messages,
    );
    crate::sha256(&serde_json::to_vec(&canonical).unwrap_or_default())
}

impl Turn<'_> {
    /// Commits one record of this turn.
    pub(super) fn journal(&self, record: &JournalRecord) -> Result<(), Error> {
        let setup = &self.agent.setup;
        setup.store.journal(setup.session, record)
    }

    /// `turn_started`, `turn_finished` and `response_received`.
    pub(super) fn journal_kind(&self, kind: JournalKind) -> Result<(), Error> {
        self.journal(&JournalRecord::new(self.id.as_str(), kind))
    }

    /// `request_sent` with `request`'s canonical hash.
    pub(super) fn journal_request(&self, request: &Request) -> Result<(), Error> {
        self.journal(
            &JournalRecord::new(self.id.as_str(), JournalKind::RequestSent)
                .with_data(request_hash(request)),
        )
    }

    /// The journal the tool port reports `calls` to.
    pub(super) fn batch_journal(&self, calls: &[RequestedCall]) -> Arc<dyn CallJournal> {
        let setup = &self.agent.setup;
        let tools = &setup.tools;
        Arc::new(BatchJournal {
            store: Arc::clone(&setup.store),
            session: setup.session,
            turn: self.id.as_str().to_owned(),
            calls: calls
                .iter()
                .map(|c| (c.id.clone(), (c.name.clone(), tools.declared(&c.name))))
                .collect(),
        })
    }
}

/// Commits the tool records of one batch.
struct BatchJournal {
    store: Arc<Store>,
    session: SessionId,
    turn: String,
    calls: HashMap<String, (String, Option<Declared>)>,
}

impl BatchJournal {
    fn commit(&self, record: &JournalRecord) {
        // The dispatcher cannot be stopped from here: a failed commit
        // only weakens a later resume (the call reads as never started
        // or unfinished), so it is logged rather than fatal.
        if let Err(err) = self.store.journal(self.session, record) {
            tracing::warn!(error = %err, "turn journal record could not be committed");
        }
    }

    fn store_result(&self, result: &ToolResult) -> Result<String, Error> {
        let bytes = serde_json::to_vec(result).unwrap_or_default();
        let blob = self.store.put_blob(&bytes)?;
        self.store
            .reference_blob(&blob, BLOB_OWNER, &self.session.to_string())?;
        Ok(blob.to_string())
    }
}

impl CallJournal for BatchJournal {
    fn started(&self, call_id: &str) {
        let mut record =
            JournalRecord::new(&self.turn, JournalKind::ToolStarted).with_call(call_id);
        if let Some((tool, declared)) = self.calls.get(call_id) {
            record.tool = Some(tool.clone());
            record.effect = declared.as_ref().map(|d| d.effect.clone());
            // An undeclared tool is not idempotent (design D15).
            record.idempotent = Some(declared.as_ref().is_some_and(|d| d.idempotent));
        }
        self.commit(&record);
    }

    fn finished(&self, call_id: &str, result: &ToolResult) {
        match self.store_result(result) {
            Ok(blob) => self.commit(
                &JournalRecord::new(&self.turn, JournalKind::ToolFinished)
                    .with_call(call_id)
                    .with_data(blob),
            ),
            Err(err) => tracing::warn!(error = %err, "journaled tool result could not be stored"),
        }
    }
}
