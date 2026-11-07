//! `cron notepad <job> [list|get|set|delete]` (spec: scheduled-jobs,
//! Per-job notepad): on the profile's store, or inside a scheduled run on
//! the run's notepad channel (its sandbox denies the home; the host applies
//! the channel to the job's notepad when the run ends).

use std::fmt::Write as _;
use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_cron::{notepad, ops};
use serde_json::{Value, json};

use super::{Env, NotepadAction};
use crate::cron_host::notepad::Page;
use crate::output::{document, print_raw};

/// `cron notepad` on the profile's store.
pub(super) fn notepad(
    env: &Env<'_>,
    key: &str,
    action: Option<&NotepadAction>,
) -> Result<u8, Error> {
    let job = ops::find(&env.store, key)?;
    let id = job.id();
    match action.unwrap_or(&NotepadAction::List) {
        NotepadAction::Set { key, value } => {
            notepad::set(&env.store, id, key, value)?;
            print_raw("")
        }
        NotepadAction::Delete { key } => {
            notepad::delete(&env.store, id, key)?;
            print_raw("")
        }
        read => show(id, &env.store.cron_notepad(id)?, read, env.ctx.json),
    }
}

/// `cron notepad` inside a scheduled run: `file` is the run's channel
/// (`PMT_CRON_NOTEPAD`), which names the only job the run may touch.
///
/// # Errors
///
/// Another job (66), a missing key (66), `notepad limit exceeded` (65,
/// the channel unchanged), an unreadable or unwritable channel.
pub fn in_run(
    file: &Path,
    key: &str,
    action: Option<&NotepadAction>,
    json: bool,
) -> Result<u8, Error> {
    let mut page = Page::read(file)?;
    if key != page.job_id && key != page.job_name {
        return Err(Error::new(
            &codes::CRON_JOB_NOT_FOUND,
            format!(
                "a scheduled run edits only its own job's notepad (job {} '{}')",
                page.job_id, page.job_name
            ),
        ));
    }
    match action.unwrap_or(&NotepadAction::List) {
        NotepadAction::Set { key, value } => {
            let mut notes = page.notes.clone();
            notes.insert(key.clone(), value.clone());
            notepad::check(&notes.into_iter().collect::<Vec<_>>())?;
            page.notes.insert(key.clone(), value.clone());
            page.write(file)?;
            print_raw("")
        }
        NotepadAction::Delete { key } => {
            if page.notes.remove(key).is_some() {
                page.write(file)?;
            }
            print_raw("")
        }
        read => {
            let notes: Vec<(String, String)> = page.notes.clone().into_iter().collect();
            show(&page.job_id, &notes, read, json)
        }
    }
}

/// `list` or `get` of job `id`'s `notes`.
fn show(
    id: &str,
    notes: &[(String, String)],
    action: &NotepadAction,
    json: bool,
) -> Result<u8, Error> {
    if let NotepadAction::Get { key } = action {
        return match notes.iter().find(|(k, _)| k == key) {
            Some((_, value)) => print_raw(&format!("{value}\n")),
            None => Err(Error::new(
                &codes::CRON_JOB_NOT_FOUND,
                format!("job {id} has no notepad key '{key}'"),
            )),
        };
    }
    if json {
        let map: serde_json::Map<String, Value> = notes
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        return document("cron notepad", &json!({"job": id, "notes": map}));
    }
    let text = notes.iter().fold(String::new(), |mut text, (k, v)| {
        let _ = writeln!(text, "{k}\t{v}");
        text
    });
    print_raw(&text)
}
