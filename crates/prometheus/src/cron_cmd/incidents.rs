//! `cron incidents [ack]` and `cron notepad` (spec: scheduled-jobs,
//! Failure incidents with alert-once and acknowledge; Per-job notepad).

use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_cron::{notepad, ops};
use pmt_store::cron::IncidentRow;
use serde_json::{Value, json};

use super::{Env, NotepadAction};
use crate::output::{document, print_raw};

const STATES: &[&str] = &["detected", "alerted", "resolved", "closed"];

fn incident_json(row: &IncidentRow) -> Value {
    let at = |ms: i64| pmt_cron::schedule::at_ms(ms).to_string();
    json!({
        "id": row.id, "job_id": row.job_id, "job_name": row.job_name, "state": row.state,
        "signature": row.signature, "error": row.error, "occurrences": row.occurrences,
        "first_seen_at": at(row.first_seen_at), "last_seen_at": at(row.last_seen_at),
        "alerted_at": row.alerted_at.map(at),
    })
}

pub(super) fn list(env: &Env<'_>, state: Option<&str>, job: Option<&str>) -> Result<u8, Error> {
    if let Some(state) = state.filter(|s| !STATES.contains(s)) {
        return Err(Error::new(
            &pmt_core::error::codes::CLI_USAGE,
            format!(
                "unknown incident state '{state}' (expected {})",
                STATES.join(", ")
            ),
        ));
    }
    let job_id = job
        .map(|key| ops::find(&env.store, key).map(|j| j.id().to_owned()))
        .transpose()?;
    let rows = env.store.cron_incidents(state, job_id.as_deref())?;
    if env.ctx.json {
        let rows: Vec<Value> = rows.iter().map(incident_json).collect();
        return document("cron incidents", &json!({"incidents": rows}));
    }
    if rows.is_empty() {
        return print_raw("no cron incidents\n");
    }
    let mut text = String::new();
    for row in &rows {
        let _ = writeln!(
            text,
            "{}  {:<8}  {} ({})  x{}  last {}\n    {}",
            row.id,
            row.state,
            row.job_name,
            row.job_id,
            row.occurrences,
            pmt_cron::schedule::at_ms(row.last_seen_at),
            row.error.lines().next().unwrap_or_default(),
        );
    }
    print_raw(&text)
}

pub(super) fn ack(env: &Env<'_>, id: &str, now: Timestamp) -> Result<u8, Error> {
    let row = env.store.cron_ack_incident(id, now.as_millisecond())?;
    if env.ctx.json {
        return document("cron incidents ack", &incident_json(&row));
    }
    print_raw(&format!(
        "acknowledged incident {} of job {}: this error stays silent\n",
        row.id, row.job_name
    ))
}

pub(super) fn notepad(
    env: &Env<'_>,
    key: &str,
    action: Option<&NotepadAction>,
) -> Result<u8, Error> {
    let job = ops::find(&env.store, key)?;
    let id = job.id();
    match action.unwrap_or(&NotepadAction::List) {
        NotepadAction::List => {
            let notes = env.store.cron_notepad(id)?;
            if env.ctx.json {
                let map: serde_json::Map<String, Value> = notes
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                return document("cron notepad", &json!({"job": id, "notes": map}));
            }
            let text = notes.iter().fold(String::new(), |mut text, (k, v)| {
                let _ = writeln!(text, "{k}\t{v}");
                text
            });
            print_raw(&text)
        }
        NotepadAction::Get { key } => {
            let notes = env.store.cron_notepad(id)?;
            match notes.iter().find(|(k, _)| k == key) {
                Some((_, value)) => print_raw(&format!("{value}\n")),
                None => Err(Error::new(
                    &pmt_core::error::codes::CRON_JOB_NOT_FOUND,
                    format!("job {id} has no notepad key '{key}'"),
                )),
            }
        }
        NotepadAction::Set { key, value } => {
            notepad::set(&env.store, id, key, value)?;
            print_raw("")
        }
        NotepadAction::Delete { key } => {
            notepad::delete(&env.store, id, key)?;
            print_raw("")
        }
    }
}
