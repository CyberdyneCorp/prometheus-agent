//! `process.list` and `process.log` (spec: desktop-app, Terminal pane
//! tools): the desktop mirrors each background process of a session in a
//! read-only terminal tab; closing the tab never stops the process.

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{ProcessEntry, ProcessList, ProcessLog, ProcessRef};
use pmt_tools::environments::Environments;

fn registry(session_id: &str) -> Option<std::sync::Arc<pmt_tools::terminal::Terminal>> {
    Environments::process()
        .session(session_id)
        .and_then(|env| env.terminal.clone())
}

/// The session's live background processes (none before its terminal
/// started one).
pub(in super::super) fn list(session_id: &str) -> ProcessList {
    let processes = registry(session_id)
        .map(|terminal| terminal.processes().mirrors())
        .unwrap_or_default()
        .into_iter()
        .map(|(process_id, command, state)| ProcessEntry {
            process_id,
            command,
            state,
        })
        .collect();
    ProcessList { processes }
}

/// One process's buffered output.
///
/// # Errors
///
/// `process.not_found` when the session has no such live process.
pub(in super::super) fn log(process: &ProcessRef) -> Result<ProcessLog, Error> {
    let (state, output) = registry(&process.session_id)
        .and_then(|terminal| terminal.processes().mirror(&process.process_id))
        .ok_or_else(|| {
            Error::new(
                &codes::PROCESS_NOT_FOUND,
                format!(
                    "no background process {} in this session",
                    process.process_id
                ),
            )
        })?;
    Ok(ProcessLog {
        process_id: process.process_id.clone(),
        state,
        output,
    })
}
