//! `prometheus trajectory show` (spec: observability, Canonical text
//! rendering): a session's recorded trajectory as canonical text, as its
//! stored JSONL lines, or with `--json` as one document.

use std::io::Read as _;
use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::render::{RenderOptions, render};
use pmt_obs::trajectory::{Line, files, read};
use pmt_store::Store;
use serde::Serialize;

use crate::cli::{TrajectoryCommand, TrajectoryFormat};
use crate::context::Context;
use crate::output::{document, print_raw};

/// Runs a `prometheus trajectory` subcommand.
///
/// # Errors
///
/// `session.not_found`/`session.ambiguous`, `trajectory.not_found` (66),
/// `trajectory.grammar_too_new` (76), `trajectory.malformed`,
/// `trajectory.io`, `output.io`.
pub fn run(command: &TrajectoryCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let TrajectoryCommand::Show {
        session,
        format,
        args_trunc,
        obs_trunc,
        max_chars,
    } = command;
    let scope = ctx.scope()?;
    let id = match SessionId::parse(session) {
        Ok(id) => id,
        Err(_) => Store::open(&scope)?.resolve_session(session)?,
    };
    let dir = scope.path(files::DIR);
    let path = files::find(&dir, &id.to_string()).ok_or_else(|| {
        Error::new(
            &codes::TRAJECTORY_NOT_FOUND,
            format!("session {id} has no trajectory in {}", dir.display()),
        )
    })?;
    let lines = read::read_file(&path)?;
    if ctx.json {
        return document(
            "trajectory show",
            &Document {
                session_id: id.to_string(),
                file: path,
                events: lines,
            },
        );
    }
    match format {
        TrajectoryFormat::Jsonl => print_raw(&raw(&path)?),
        TrajectoryFormat::Text => {
            let options = RenderOptions {
                args_trunc: *args_trunc,
                obs_trunc: *obs_trunc,
                max_chars: *max_chars,
            };
            print_raw(&render(&lines, &options)?)
        }
    }
}

#[derive(Serialize)]
struct Document {
    session_id: String,
    file: PathBuf,
    events: Vec<Line>,
}

fn raw(path: &std::path::Path) -> Result<String, Error> {
    let mut text = String::new();
    files::open(path)
        .and_then(|mut reader| reader.read_to_string(&mut text))
        .map_err(|err| {
            Error::new(
                &codes::TRAJECTORY_IO,
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
    Ok(text)
}
