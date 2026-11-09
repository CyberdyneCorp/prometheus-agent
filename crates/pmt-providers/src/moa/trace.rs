//! MoA traces (spec: mixture-of-agents, MoA traces): with
//! `moa.save_traces = true`, one JSONL record per MoA model call in
//! `<profile>/moa-traces/<session_id>.jsonl` (or under `moa.trace_dir`),
//! holding each slot's request summary, output, usage and error after the
//! privacy filter and known-secret redaction.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::schema::MoaConfig;
use serde::Serialize;

use crate::event::Usage;
use crate::request::Request;
use crate::resolve::Route;

/// What a slot was sent.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Messages in the request.
    pub messages: usize,
    /// System prompt blocks (none for references).
    pub system_blocks: usize,
    /// Tool schemas (none for references).
    pub tools: usize,
    /// The reasoning effort sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<&'static str>,
    /// The temperature sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// The canonical request hash (the ledger's).
    pub request_hash: String,
}

impl Summary {
    /// The summary of `request` sent on `route`.
    #[must_use]
    pub fn of(request: &Request, route: &Route, temperature: Option<f64>) -> Self {
        Self {
            messages: request.messages.len(),
            system_blocks: request.system.len(),
            tools: request.tools.len(),
            reasoning: request
                .reasoning
                .map(pmt_config::schema::ReasoningLevel::as_str),
            temperature,
            request_hash: request.hash(&route.provider, &route.model),
        }
    }
}

/// One slot's entry.
#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// What it was sent (`None` when its route did not resolve).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Summary>,
    /// What it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Tools it called (the aggregator).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// Its usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Slot {
    /// An entry for `provider`/`model` with nothing known yet.
    #[must_use]
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            request: None,
            output: None,
            tool_calls: Vec::new(),
            usage: None,
            error: None,
        }
    }
}

/// One MoA model call.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// When the call started (RFC 3339).
    pub at: String,
    /// The session.
    pub session_id: String,
    /// The turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// The preset.
    pub preset: String,
    /// The model iteration within the turn (1-based).
    pub iteration: u32,
    /// The references run for this call (empty when the turn's earlier
    /// guidance was reused).
    pub references: Vec<Slot>,
    /// The aggregator.
    pub aggregator: Slot,
}

/// The trace file of `session` under `config` (relative to
/// `profile_dir`); `None` when traces are off or there is no profile.
#[must_use]
pub fn file(config: &MoaConfig, profile_dir: Option<&Path>, session: &str) -> Option<PathBuf> {
    if !config.save_traces {
        return None;
    }
    Some(
        config
            .trace_dir(profile_dir?)
            .join(format!("{session}.jsonl")),
    )
}

/// Appends `record` to `path` (owner-only), logging a failure: a trace
/// never fails the call.
pub fn append(path: &Path, record: &Record) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            pmt_core::private_fs::create_dir(dir)?;
        }
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        pmt_core::private_fs::append_options()
            .open(path)?
            .write_all(&line)
    };
    if let Err(err) = write() {
        tracing::warn!(path = %path.display(), "cannot write the MoA trace: {err}");
    }
}
