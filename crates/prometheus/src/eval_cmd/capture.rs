//! `prometheus eval capture <session|recording> --suite DIR` (spec:
//! replay-and-evaluation, Session-to-task capture): the session's
//! recording (exported with its workspace when the session is in this
//! profile, or a `.pmtrec` file) gives the prompt and the task files;
//! the user gives the grader.

use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::capture::{Capture, Expect};
use pmt_eval::recording::{Recording, member};
use pmt_store::Store;
use serde_json::{Value, json};

use crate::context::Context;

/// `prometheus eval capture`.
#[derive(Debug, Clone, Args)]
#[command(group(ArgGroup::new("grader").required(true).args(["expect", "expect_regex", "grader_script"])))]
pub struct CaptureArgs {
    /// The session (id, prefix or title) or a `.pmtrec` recording.
    pub session: String,
    /// The suite to add the task to (created when it has no suite.toml).
    #[arg(long, value_name = "DIR")]
    pub suite: PathBuf,
    /// The new task's id (default: `session-<id suffix>`).
    #[arg(long, value_name = "ID")]
    pub task_id: Option<String>,
    /// An `exact` grader expecting this answer.
    #[arg(long, value_name = "TEXT")]
    pub expect: Option<String>,
    /// A `regex` grader.
    #[arg(long, value_name = "RE")]
    pub expect_regex: Option<String>,
    /// A `script` grader (copied into the task's grader/).
    #[arg(long, value_name = "PATH")]
    pub grader_script: Option<PathBuf>,
}

impl CaptureArgs {
    fn expect(&self) -> Expect {
        match (&self.expect, &self.expect_regex, &self.grader_script) {
            (Some(text), _, _) => Expect::Exact(text.clone()),
            (_, Some(re), _) => Expect::Regex(re.clone()),
            (_, _, Some(path)) => Expect::Script(path.clone()),
            (None, None, None) => unreachable!("clap requires one grader"),
        }
    }
}

/// The recording `spec` names: a `.pmtrec` file, else a session of the
/// profile exported with its workspace.
fn recording(spec: &str, ctx: &Context<'_>) -> Result<Recording, Error> {
    let path = Path::new(spec);
    if path.is_file() {
        return Recording::open(path);
    }
    let scope = ctx.scope()?;
    let store = Store::open(&scope)?;
    let id = SessionId::parse(spec).or_else(|_| store.resolve_session(spec))?;
    crate::replay::export::recording(
        &scope,
        &store,
        id,
        crate::replay::export::Options {
            include_workspace: true,
            lineage: false,
        },
    )
}

/// The first user message of the recording's first session.
fn first_prompt(rec: &Recording) -> Result<String, Error> {
    let session = rec.root().unwrap_or_default().to_owned();
    let text = rec
        .get(&member::trajectory(&session))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|l| l["type"] == "user.message")
        .and_then(|l| l["payload"]["text"].as_str().map(str::to_owned))
        .ok_or_else(|| {
            Error::new(
                &codes::EVAL_CAPTURE_REFUSED,
                format!("session {session} has no user message"),
            )
        })
}

/// The manifest's files with their recorded contents, if any.
fn files(rec: &Recording) -> Vec<(String, Option<Vec<u8>>)> {
    let manifest: Value = rec
        .json(member::WORKSPACE)
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    manifest["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["path"].as_str())
        .map(|path| {
            (
                path.to_owned(),
                rec.get(&member::workspace_file(path)).map(<[u8]>::to_vec),
            )
        })
        .collect()
}

/// Runs `prometheus eval capture`.
///
/// # Errors
///
/// Session and recording errors; `eval.capture_refused` (65).
pub fn run(args: &CaptureArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let rec = recording(&args.session, ctx)?;
    let session = rec.root().unwrap_or_default().to_owned();
    let task_id = args.task_id.clone().unwrap_or_else(|| {
        let tail: String = session
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("session-{}", tail.to_lowercase())
    });
    let capture = Capture {
        suite_dir: args.suite.clone(),
        task_id,
        prompt: first_prompt(&rec)?,
        files: files(&rec),
        expect: args.expect(),
    };
    let dir = pmt_eval::capture::write(&capture)?;
    if ctx.json {
        return crate::output::document(
            "eval capture",
            &json!({"task": dir, "files": capture.files.len()}),
        );
    }
    crate::output::print(&format!("captured session {session} as {}", dir.display()))
}
