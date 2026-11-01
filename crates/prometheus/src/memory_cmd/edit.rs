//! `prometheus memory edit`: the Markdown view in the user's editor,
//! applied as one batch after the diff is shown (spec: memory, Memory CLI
//! and editing). A draft that does not read, or that the scan refuses,
//! changes nothing.

use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_memory::{manage, markdown};
use pmt_store::MemoryTarget;

use super::Env;
use crate::config_cmd::edit::{DEFAULT_EDITOR, launch_editor};
use crate::output::json;

pub(super) fn run(env: &Env, launch: &LaunchEnv, targets: &[MemoryTarget]) -> Result<u8, Error> {
    let entries: Vec<_> = env
        .store
        .memory_entries(None)?
        .into_iter()
        .filter(|e| targets.contains(&e.target))
        .collect();
    let original = markdown::render(&entries, targets);
    let draft = env.scope.dir().join("memory.edit.md");
    let io_error =
        |e: std::io::Error| Error::new(&codes::HOME_IO, format!("{}: {e}", draft.display()));
    private_fs::write_atomic(&draft, original.as_bytes()).map_err(io_error)?;
    let edited = launch_editor(launch.editor().unwrap_or(DEFAULT_EDITOR), &draft)
        .and_then(|()| std::fs::read_to_string(&draft).map_err(io_error));
    let _ = std::fs::remove_file(&draft);
    let edited = edited?;
    if edited == original {
        eprintln!("no changes");
        return Ok(0);
    }
    let sections = markdown::parse(&edited).map_err(|m| Error::new(&codes::MEMORY_INVALID, m))?;
    eprint!("{}", markdown::diff(&original, &edited));
    let redactor = env.scope.secrets().redactor()?;
    let summary = manage::apply_edit(
        &env.store,
        &env.scope.config().memory,
        targets,
        &sections,
        &redactor,
    )?;
    if env.json {
        return json(&summary);
    }
    eprintln!(
        "applied: {} added, {} changed, {} removed (from the next session)",
        summary.added, summary.updated, summary.removed
    );
    Ok(0)
}
