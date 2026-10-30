//! `prometheus logs` (spec: cli, Log viewer; observability, Log viewer):
//! the last records of one component, filtered by level, time and session,
//! optionally followed until interrupted. The header names every file read.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_core::error::{Error, ErrorKind};
use pmt_core::id::SessionId;
use pmt_obs::logs::view::{self, Filter, Follower};
use pmt_obs::logs::{self, COMPONENTS, Record};
use pmt_store::Store;
use serde::Serialize;

use crate::cli::LogsArgs;
use crate::context::Context;
use crate::output::{document, print_raw, write_json};

/// How often `--follow` looks for new records.
const POLL: Duration = Duration::from_millis(200);

/// Runs `prometheus logs`.
///
/// # Errors
///
/// `logs.invalid_since` (2), home and profile errors, `session.not_found`
/// for `--root`, `logs.io`, `output.io`.
pub fn run(args: &LogsArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let component = args.component.as_deref().unwrap_or(logs::DEFAULT_COMPONENT);
    let since = args
        .since
        .as_deref()
        .map(|value| view::parse_since(value, jiff::Timestamp::now()))
        .transpose()?;
    let home = ctx.home()?;
    let profile = ctx.profile_dir()?;
    if component == "list" {
        return list(home.root(), profile.path(), ctx.json);
    }
    let filter = Filter {
        component: component.to_owned(),
        level: args.level.map(|l| l.as_str().to_owned()),
        since,
        sessions: sessions(args, ctx)?,
    };
    let current = view::current_file(home.root(), profile.path(), component);
    // Taken before printing so records written meanwhile are followed.
    let follower = args.follow.then(|| Follower::from_end(&current));
    let (records, mut read) = view::tail(&current, &filter, args.lines)?;
    if read.is_empty() {
        read.push(current.clone());
    }
    if ctx.json && !args.follow {
        return document(
            "logs",
            &LogsDocument {
                files: &read,
                records: &records,
            },
        );
    }
    print_header(&read, ctx.json)?;
    for record in &records {
        print_record(record, ctx.json)?;
    }
    match follower {
        Some(follower) => follow(follower, &filter, ctx.json),
        None => Ok(0),
    }
}

#[derive(Serialize)]
struct LogsDocument<'a> {
    files: &'a [PathBuf],
    records: &'a [Record],
}

/// `--session` (an id, else resolved like other session references) or
/// every session of `--root`'s lineage.
fn sessions(args: &LogsArgs, ctx: &Context<'_>) -> Result<Option<BTreeSet<String>>, Error> {
    if let Some(reference) = &args.session {
        let id = match SessionId::parse(reference) {
            Ok(id) => id,
            Err(_) => Store::open(&ctx.scope()?)?.resolve_session(reference)?,
        };
        return Ok(Some(BTreeSet::from([id.to_string()])));
    }
    let Some(reference) = &args.root else {
        return Ok(None);
    };
    let store = Store::open(&ctx.scope()?)?;
    let root = store
        .session(store.resolve_session(reference)?)?
        .root_session_id;
    let mut ids = BTreeSet::from([root.to_string()]);
    ids.extend(
        store
            .tree(root)?
            .into_iter()
            .map(|node| node.id.to_string()),
    );
    Ok(Some(ids))
}

fn list(home: &Path, profile: &Path, json: bool) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Component {
        name: &'static str,
        file: PathBuf,
    }
    let components: Vec<Component> = COMPONENTS
        .iter()
        .map(|name| Component {
            name,
            file: view::current_file(home, profile, name),
        })
        .collect();
    if json {
        #[derive(Serialize)]
        struct Body {
            components: Vec<Component>,
        }
        return document("logs list", &Body { components });
    }
    let mut text = String::new();
    for c in &components {
        let _ = writeln!(text, "{:<10} {}", c.name, c.file.display());
    }
    print_raw(&text)
}

fn print_header(files: &[PathBuf], json: bool) -> Result<u8, Error> {
    if json {
        #[derive(Serialize)]
        struct Header<'a> {
            schema: &'static str,
            files: &'a [PathBuf],
        }
        return emit_json(&Header {
            schema: "prometheus.logs.v1",
            files,
        });
    }
    let mut text = String::new();
    for path in files {
        let _ = writeln!(text, "==> {} <==", path.display());
    }
    print_raw(&text)
}

fn print_record(record: &Record, json: bool) -> Result<u8, Error> {
    if json {
        return emit_json(record);
    }
    print_raw(&format!("{}\n", format_record(record)))
}

fn emit_json(value: &impl Serialize) -> Result<u8, Error> {
    write_json(&mut std::io::stdout().lock(), value).map_err(|err| {
        Error::new(
            &pmt_core::error::codes::OUTPUT_IO,
            format!("cannot write to stdout: {err}"),
        )
    })?;
    Ok(0)
}

/// `<ts> <LEVEL> <component> [<session>]: <msg> [k=v ...]`.
#[must_use]
pub fn format_record(record: &Record) -> String {
    let mut line = format!(
        "{} {:<5} {}",
        record.ts,
        record.level.to_uppercase(),
        record.component
    );
    if let Some(session) = &record.session_id {
        let _ = write!(line, " {session}");
    }
    let _ = write!(line, ": {}", record.msg);
    for (key, value) in &record.fields {
        match value {
            serde_json::Value::String(text) => {
                let _ = write!(line, " {key}={text}");
            }
            other => {
                let _ = write!(line, " {key}={other}");
            }
        }
    }
    line
}

/// Prints new matching records until Ctrl-C, then returns exit 130. With
/// `--json` each record is one JSON line after the header line.
fn follow(mut follower: Follower, filter: &Filter, json: bool) -> Result<u8, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            Error::new(
                &pmt_core::error::codes::LOGS_IO,
                format!("cannot start the follow loop: {err}"),
            )
        })?;
    runtime.block_on(async {
        let interrupted = tokio::signal::ctrl_c();
        tokio::pin!(interrupted);
        let mut ticker = tokio::time::interval(POLL);
        loop {
            tokio::select! {
                _ = &mut interrupted => return Ok(ErrorKind::Cancelled.exit_code()),
                _ = ticker.tick() => {
                    for record in follower.poll()?.iter().filter(|r| filter.matches(r)) {
                        print_record(record, json)?;
                    }
                }
            }
        }
    })
}
