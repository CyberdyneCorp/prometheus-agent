//! The `experience.*` methods in the daemon (spec: experience-library,
//! Library CLI): each does what the matching subcommand does.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_evolve::experience::entry::EntryMeta;
use pmt_evolve::experience::{Library, ops};
use pmt_gateway::experience::ExperienceRequest;
use serde_json::{Value, json};

fn reply(message: &str, meta: &EntryMeta) -> Value {
    json!({"message": message, "data": meta})
}

/// Runs one method for `scope`.
///
/// # Errors
///
/// The matching command's errors; `cli.usage` without a needed
/// `entry_id`.
pub fn handle(scope: &ProfileScope, request: &ExperienceRequest) -> Result<Value, Error> {
    let p = &request.params;
    let dir = scope.dir();
    let entry = || {
        p.entry_id
            .as_deref()
            .ok_or_else(|| Error::new(&codes::CLI_USAGE, "entry_id is required"))
    };
    match request.method {
        "experience.list" => {
            let entries = super::listed(dir, p.domain.as_deref(), p.all)?;
            Ok(json!({"entries": entries}))
        }
        "experience.domains" => {
            let domains: Vec<Value> = super::domain_counts(dir)
                .into_iter()
                .map(|(name, entries)| json!({"name": name, "entries": entries}))
                .collect();
            Ok(json!({"domains": domains}))
        }
        "experience.show" => {
            let library = Library::of(dir);
            let meta = library.find(entry()?)?;
            let mut data = serde_json::to_value(&meta).unwrap_or_default();
            data["document"] = Value::String(library.document(&meta));
            Ok(json!({"message": format!("{} ({})", meta.entry_id, meta.domain), "data": data}))
        }
        "experience.pin" => {
            let pinned = p.pinned.unwrap_or(true);
            let meta = ops::pin(dir, entry()?, pinned)?;
            let what = if pinned { "pinned" } else { "unpinned" };
            Ok(reply(&format!("{} {what}", meta.entry_id), &meta))
        }
        "experience.forget" => {
            let meta = ops::forget(dir, entry()?)?;
            Ok(reply(&format!("{} forgotten", meta.entry_id), &meta))
        }
        "experience.disable" => {
            let meta = ops::disable(dir, entry()?)?;
            Ok(reply(&format!("{} disabled", meta.entry_id), &meta))
        }
        "experience.restore" => {
            let meta = ops::restore(dir, entry()?)?;
            Ok(reply(&format!("{} restored", meta.entry_id), &meta))
        }
        other => Err(Error::new(
            &codes::RPC_NOT_SERVED,
            format!("{other} is not served"),
        )),
    }
}
