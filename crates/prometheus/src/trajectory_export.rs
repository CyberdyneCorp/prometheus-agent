//! `prometheus trajectory export --format sharegpt|jsonl [--session ID
//! ... | --eval-run ID | --since S] [--include-failed] -o FILE` (spec:
//! observability, `ShareGPT` trajectory export): one JSON line per session,
//! redacted with the profile's known secrets and credential patterns.
//! Sessions that ended in an error are left out unless
//! `--include-failed`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::{Line, files, read};
use pmt_store::{ListOptions, Source, Store};
use serde::Serialize;
use serde_json::{Value, json};

use crate::context::Context;

/// `trajectory export --format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    /// `ShareGPT` `conversations` records.
    Sharegpt,
    /// The session's redacted trajectory events, one record per session.
    Jsonl,
}

/// `prometheus trajectory export` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct ExportArgs {
    /// Record format.
    #[arg(long, value_enum)]
    pub format: ExportFormat,
    /// Sessions to export (id, id prefix or title; repeatable).
    #[arg(long, value_name = "ID", conflicts_with_all = ["eval_run", "since"])]
    pub session: Vec<String>,
    /// Sessions of this evaluation run.
    #[arg(long, value_name = "ID", conflicts_with = "since")]
    pub eval_run: Option<String>,
    /// Sessions started since `<N>[smhd]` ago or an RFC 3339 time.
    #[arg(long, value_name = "S")]
    pub since: Option<String>,
    /// Also sessions that ended in an error.
    #[arg(long)]
    pub include_failed: bool,
    /// The JSON-lines file to write.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: PathBuf,
}

#[derive(Serialize)]
struct Written {
    path: PathBuf,
    exported: usize,
    skipped_failed: usize,
}

/// Runs `prometheus trajectory export`.
///
/// # Errors
///
/// Session resolution errors, `logs.invalid_since` (2),
/// `trajectory.not_found` (66) for an asked session without a
/// trajectory, `trajectory.io` writing the file.
pub fn run(args: &ExportArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Store::open(&scope)?;
    let ids = selection(args, &store)?;
    let redactor = scope.secrets().redactor()?;
    let dir = scope.path(files::DIR);
    let mut out = String::new();
    let (mut exported, mut skipped) = (0, 0);
    for id in ids {
        let Some(path) = files::find(&dir, &id.to_string()) else {
            if !args.session.is_empty() {
                return Err(Error::new(
                    &codes::TRAJECTORY_NOT_FOUND,
                    format!("session {id} has no trajectory in {}", dir.display()),
                ));
            }
            continue;
        };
        let lines = read::read_file(&path)?;
        if !args.include_failed && !pmt_obs::sharegpt::completed(&lines) {
            skipped += 1;
            continue;
        }
        let record = match args.format {
            ExportFormat::Sharegpt => sharegpt(&scope, &id, &lines),
            ExportFormat::Jsonl => events(&id, &lines),
        };
        out.push_str(&redactor.redact_json(record).to_string());
        out.push('\n');
        exported += 1;
    }
    write(&args.output, &out)?;
    let written = Written {
        path: args.output.clone(),
        exported,
        skipped_failed: skipped,
    };
    if ctx.json {
        return crate::output::document("trajectory export", &written);
    }
    crate::output::print(&format!(
        "exported {exported} session(s) to {} ({skipped} that ended in an error left out)",
        args.output.display()
    ))
}

/// The sessions to export, oldest first.
fn selection(args: &ExportArgs, store: &Store) -> Result<Vec<SessionId>, Error> {
    if !args.session.is_empty() {
        return args
            .session
            .iter()
            .map(|s| SessionId::parse(s).or_else(|_| store.resolve_session(s)))
            .collect();
    }
    let since = args
        .since
        .as_deref()
        .map(|s| pmt_obs::logs::view::parse_since(s, jiff::Timestamp::now()))
        .transpose()?;
    let mut sessions = store.list_sessions(&ListOptions {
        all: true,
        ..ListOptions::default()
    })?;
    sessions.retain(|s| since.is_none_or(|since| s.started_at >= since));
    if let Some(run) = &args.eval_run {
        sessions
            .retain(|s| s.source == Source::Eval && s.source_tag.as_deref() == Some(run.as_str()));
        if sessions.is_empty() {
            return Err(Error::new(
                &codes::SESSION_NOT_FOUND,
                format!("no session of eval run {run}"),
            ));
        }
    }
    sessions.sort_by_key(|s| s.started_at);
    Ok(sessions.into_iter().map(|s| s.id).collect())
}

/// The `ShareGPT` record of a session: its frozen system prompt and tool
/// definitions come from the session's recording, when it has one.
fn sharegpt(scope: &ProfileScope, id: &SessionId, lines: &[Line]) -> Value {
    let journal = pmt_obs::recording::read(scope.dir(), &id.to_string())
        .ok()
        .flatten();
    let system = journal.as_ref().and_then(|j| j.prompts.first()).map(|p| {
        p.sections
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    let tools = journal
        .as_ref()
        .and_then(|j| j.calls.first())
        .and_then(|c| c.request["tools"].as_array().cloned())
        .unwrap_or_default();
    let toolsets = journal
        .as_ref()
        .and_then(|j| j.header.as_ref())
        .and_then(|h| h["config"].as_str())
        .and_then(|config| toml::from_str::<toml::Value>(config).ok())
        .and_then(|config| {
            config
                .get("tools")?
                .get("enabled_toolsets")?
                .as_array()
                .cloned()
        })
        .map(|list| {
            list.iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let record = pmt_obs::sharegpt::record(lines, system.as_deref(), tools, toolsets);
    serde_json::to_value(record).unwrap_or(Value::Null)
}

/// The `jsonl` record: the session's events.
fn events(id: &SessionId, lines: &[Line]) -> Value {
    json!({
        "session_id": id.to_string(),
        "completed": pmt_obs::sharegpt::completed(lines),
        "events": lines,
    })
}

fn write(path: &Path, text: &str) -> Result<(), Error> {
    let failed = |e: std::io::Error| Error::io(&codes::TRAJECTORY_IO, path, &e);
    let mut file = std::fs::File::create(path).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)
}
