//! `chat --usage-file <path>` (spec: cli, Usage report file): when the run
//! ends, for any reason, one JSON document with the run's usage is written
//! atomically to the path, independent of stdout and `--json`. A path that
//! cannot be written exits 73.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_providers::resolve::Target;
use pmt_store::{CostStatus, Store, UsageEvent};
use serde::{Deserialize, Serialize};

/// What a run did, filled in as it goes.
#[derive(Debug)]
pub struct RunRecord {
    started: Timestamp,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    store: Option<Arc<Store>>,
    sessions: Vec<SessionId>,
    root: Option<SessionId>,
    target: Option<Target>,
    turns: u32,
    /// The reports of turns a gateway daemon ran (an attached chat).
    remote: Vec<Report>,
}

impl Default for RunRecord {
    fn default() -> Self {
        Self {
            started: Timestamp::now(),
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl RunRecord {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The profile's store is open.
    pub fn store(&self, store: &Arc<Store>) {
        self.lock().store = Some(Arc::clone(store));
    }

    /// Session `id` (of lineage `root`) runs on `target`.
    pub fn session(&self, id: SessionId, root: SessionId, target: &Target) {
        let mut inner = self.lock();
        if !inner.sessions.contains(&id) {
            inner.sessions.push(id);
        }
        inner.root = Some(root);
        inner.target = Some(target.clone());
    }

    /// A turn starts.
    pub fn turn(&self) {
        self.lock().turns += 1;
    }

    /// A turn the gateway daemon ran, with its report (`turn.finished`
    /// `report`); unreadable reports are skipped.
    pub fn remote_turn(&self, report: &serde_json::Value) {
        if let Ok(report) = serde_json::from_value::<Report>(report.clone()) {
            self.lock().remote.push(report);
        }
    }
}

/// The document.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Report {
    /// The newest session of the run.
    pub session_id: Option<String>,
    /// Its lineage root.
    pub root_session_id: Option<String>,
    /// The model of the main route.
    pub model: Option<String>,
    /// Its provider.
    pub provider: Option<String>,
    /// Who paid for the main calls.
    pub billing_source: Option<String>,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Cost in USD of the priced calls.
    pub cost: f64,
    /// How the cost is known (`exact`, `estimated`, `subscription`,
    /// `unknown`).
    pub cost_status: String,
    /// Turns started.
    pub turns: u32,
    /// Why the run ended.
    pub exit_reason: String,
    /// The exit status.
    pub exit_code: u8,
}

impl RunRecord {
    /// The report of a run that ended with `outcome`.
    #[must_use]
    pub fn report(&self, outcome: &Result<u8, Error>) -> Report {
        let inner = self.lock();
        if inner.store.is_none() && !inner.remote.is_empty() {
            return merged(&inner.remote, outcome);
        }
        let events = run_events(&inner, self.started);
        let mut report = Report {
            session_id: inner.sessions.last().map(ToString::to_string),
            root_session_id: inner.root.map(|r| r.to_string()),
            model: inner.target.as_ref().and_then(|t| t.model.clone()),
            provider: inner.target.as_ref().map(|t| t.provider.clone()),
            billing_source: events
                .iter()
                .rev()
                .find(|e| e.purpose == "main")
                .or_else(|| events.last())
                .map(|e| e.billing_source.clone()),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            cost: 0.0,
            cost_status: cost_status(&events).to_owned(),
            turns: inner.turns,
            exit_reason: exit_reason(outcome),
            exit_code: match outcome {
                Ok(code) => *code,
                Err(err) => err.exit_code(),
            },
        };
        for event in &events {
            let t = &event.tokens;
            report.input_tokens += t.input;
            report.output_tokens += t.output;
            report.cache_read_tokens += t.cache_read;
            report.cache_write_tokens += t.cache_write;
            report.reasoning_tokens += t.reasoning;
            report.cost += event.cost_usd.unwrap_or(0.0);
        }
        report
    }
}

/// The reports of the turns a daemon ran, as one run that ended with
/// `outcome`: the last turn's session and route, the sums of tokens and
/// cost, the weakest cost status.
fn merged(turns: &[Report], outcome: &Result<u8, Error>) -> Report {
    let last = turns.last().expect("at least one turn");
    let has = |status: &str| turns.iter().any(|t| t.cost_status == status);
    let cost_status = if has("unknown") {
        "unknown"
    } else if has("estimated") {
        "estimated"
    } else if turns.iter().all(|t| t.cost_status == "subscription") {
        "subscription"
    } else {
        "exact"
    };
    let mut report = Report {
        session_id: last.session_id.clone(),
        root_session_id: last.root_session_id.clone(),
        model: last.model.clone(),
        provider: last.provider.clone(),
        billing_source: turns.iter().rev().find_map(|t| t.billing_source.clone()),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cost: 0.0,
        cost_status: cost_status.to_owned(),
        turns: u32::try_from(turns.len()).unwrap_or(u32::MAX),
        exit_reason: exit_reason(outcome),
        exit_code: match outcome {
            Ok(code) => *code,
            Err(err) => err.exit_code(),
        },
    };
    for turn in turns {
        report.input_tokens += turn.input_tokens;
        report.output_tokens += turn.output_tokens;
        report.cache_read_tokens += turn.cache_read_tokens;
        report.cache_write_tokens += turn.cache_write_tokens;
        report.reasoning_tokens += turn.reasoning_tokens;
        report.cost += turn.cost;
    }
    report
}

/// The run's model calls: the sessions' events since the run started.
fn run_events(inner: &Inner, started: Timestamp) -> Vec<UsageEvent> {
    let Some(store) = &inner.store else {
        return Vec::new();
    };
    inner
        .sessions
        .iter()
        .filter_map(|id| store.usage_events(*id).ok())
        .flatten()
        .filter(|e| e.ts >= started)
        .collect()
}

/// The weakest cost status of the calls (`exact` with no call).
fn cost_status(events: &[UsageEvent]) -> &'static str {
    let has = |status: CostStatus| events.iter().any(|e| e.cost_status == status);
    if has(CostStatus::Unknown) {
        "unknown"
    } else if has(CostStatus::Estimated) {
        "estimated"
    } else if !events.is_empty()
        && events
            .iter()
            .all(|e| e.cost_status == CostStatus::Subscription)
    {
        "subscription"
    } else {
        "exact"
    }
}

/// `completed`, the stop that ended the run (named by its exit status), or
/// the failure's error code.
fn exit_reason(outcome: &Result<u8, Error>) -> String {
    match outcome {
        Ok(0) => "completed".to_owned(),
        Ok(code) => pmt_core::exit::lookup(*code)
            .map_or("failure", |e| e.name)
            .to_owned(),
        Err(err) => err.code().code.to_owned(),
    }
}

/// Writes the report of `outcome` to `path` and returns the run's result,
/// or exit 73 (after printing the run's own error) when the file cannot
/// be written.
///
/// # Errors
///
/// The run's own error, unchanged.
///
/// # Panics
///
/// Never: the report is plain data and always serializes.
pub fn finish(
    path: &Path,
    record: &RunRecord,
    outcome: Result<u8, Error>,
    json: bool,
) -> Result<u8, Error> {
    let report = record.report(&outcome);
    let mut bytes = serde_json::to_vec_pretty(&report).expect("a report serializes");
    bytes.push(b'\n');
    match pmt_core::private_fs::write_atomic(path, &bytes) {
        Ok(()) => outcome,
        Err(e) => {
            if let Err(err) = &outcome {
                crate::output::print_error(err, json);
            }
            let err = Error::new(
                &codes::CHAT_USAGE_FILE,
                format!("cannot write --usage-file {}: {e}", path.display()),
            );
            crate::output::print_error(&err, json);
            Ok(pmt_core::exit::CANNOT_CREATE_OUTPUT)
        }
    }
}
