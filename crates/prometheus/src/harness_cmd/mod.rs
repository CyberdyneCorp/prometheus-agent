//! `prometheus harness` (spec: harness-evolution, Harness validation;
//! Harness history and diff; Rollback and manual edits). These commands
//! work whether or not `evolve.enabled` is set: inspecting, editing and
//! rolling back a harness is the user's own business.

mod args;

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::harness::{HarnessDir, Source, Tree, VersionInfo};
use pmt_core::error::{Error, codes};
use pmt_evolve::harness::{diff, validate};
use serde_json::json;

pub use args::HarnessCommand;

use crate::context::Context;
use crate::output::{json, print_raw};

/// Runs one `harness` subcommand.
///
/// # Errors
///
/// `harness.invalid` (65), `harness.version_not_found` (66),
/// `harness.io` (74).
pub fn run(command: &HarnessCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let harness = HarnessDir::of(scope.dir());
    match command {
        HarnessCommand::Validate { dir } => validate_dir(ctx, &scope, &harness, dir.as_deref()),
        HarnessCommand::Log => log(ctx, &harness),
        HarnessCommand::Show { version, path } => {
            show(ctx, &harness, version.as_deref(), path.as_deref())
        }
        HarnessCommand::Diff { a, b } => diff_versions(ctx, &harness, a, b.as_deref()),
        HarnessCommand::Rollback { version, now } => {
            let target = harness.resolve(version)?;
            let tree = harness.tree(&target.id)?;
            let written = harness.write(&tree, Source::Rollback, None)?;
            activate(ctx, &scope, &written, *now)
        }
        HarnessCommand::Apply { dir, now } => {
            let tree = checked(ctx, &scope, dir)?;
            commit_manual(ctx, &scope, &harness, &tree, *now)
        }
        HarnessCommand::Edit { path, now } => edit(ctx, &scope, &harness, path, *now),
    }
}

/// The tree under `dir`, validated (and trigger-repaired).
fn checked(ctx: &Context<'_>, scope: &ProfileScope, dir: &Path) -> Result<Tree, Error> {
    let (mut tree, problems) =
        Tree::read(dir).map_err(|e| Error::io(&codes::HARNESS_IO, dir, &e))?;
    let rules = crate::harness_host::rules(scope, ctx.launch.user_home().map(AsRef::as_ref));
    let mut errors = problems;
    match validate(&mut tree, &rules) {
        Ok(_) if errors.is_empty() => Ok(tree),
        Ok(_) => Err(invalid(&errors)),
        Err(more) => {
            errors.extend(more);
            Err(invalid(&errors))
        }
    }
}

fn invalid(errors: &[String]) -> Error {
    Error::new(&codes::HARNESS_INVALID, errors.join("\n"))
}

fn validate_dir(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    harness: &HarnessDir,
    dir: Option<&Path>,
) -> Result<u8, Error> {
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => harness.tree_dir(&harness.active_id()?),
    };
    let (mut tree, problems) =
        Tree::read(&dir).map_err(|e| Error::io(&codes::HARNESS_IO, &dir, &e))?;
    let rules = crate::harness_host::rules(scope, ctx.launch.user_home().map(AsRef::as_ref));
    let validated = match (validate(&mut tree, &rules), problems.is_empty()) {
        (Ok(validated), true) => validated,
        (Ok(_), false) => return Err(invalid(&problems)),
        (Err(errors), _) => return Err(invalid(&[problems, errors].concat())),
    };
    if ctx.json {
        return json(&json!({
            "valid": true,
            "census": validated.census,
            "repaired": validated.repaired,
        }));
    }
    let mut text = format!("valid: {}\n", census_line(&validated.census));
    for skill in &validated.repaired {
        let _ = writeln!(
            text,
            "GUARDRAILS.md gains the trigger line of skill '{skill}'"
        );
    }
    print_raw(&text)
}

fn census_line(census: &pmt_config::harness::Census) -> String {
    format!(
        "{} files, {} always-on chars, {} skills, {} agents, {} scripts, {} hooks",
        census.files,
        census.always_on_chars,
        census.skills,
        census.agents,
        census.scripts,
        census.hooks
    )
}

fn log(ctx: &Context<'_>, harness: &HarnessDir) -> Result<u8, Error> {
    let active = harness.active_id()?;
    let versions = harness.versions()?;
    if ctx.json {
        let rows: Vec<_> = versions
            .iter()
            .map(|v| json!({"active": v.id == active, "version": v}))
            .collect();
        return json(&rows);
    }
    let mut text = String::new();
    for v in &versions {
        let mark = if v.id == active { '*' } else { ' ' };
        let _ = writeln!(
            text,
            "{mark} {:<24} {:<8} job {:<30} {}  {}",
            v.id,
            v.source.as_str(),
            v.job_id.as_deref().unwrap_or("-"),
            v.created_at,
            census_line(&v.census)
        );
    }
    print_raw(&text)
}

fn show(
    ctx: &Context<'_>,
    harness: &HarnessDir,
    version: Option<&str>,
    path: Option<&str>,
) -> Result<u8, Error> {
    let info = match version {
        Some(v) => harness.resolve(v)?,
        None => harness.active()?,
    };
    let tree = harness.tree(&info.id)?;
    if let Some(path) = path {
        let text = tree.get(path).ok_or_else(|| {
            Error::new(
                &codes::HARNESS_VERSION_NOT_FOUND,
                format!("harness version {} has no file '{path}'", info.id),
            )
        })?;
        return print_raw(text);
    }
    if ctx.json {
        let files: Vec<_> = tree
            .files
            .iter()
            .map(|(p, t)| json!({"path": p, "bytes": t.len()}))
            .collect();
        return json(&json!({"version": info, "files": files}));
    }
    let mut text = format!("{} ({})\n", info.id, info.source.as_str());
    for (p, t) in &tree.files {
        let _ = writeln!(text, "  {p}  {} bytes", t.len());
    }
    print_raw(&text)
}

fn diff_versions(
    ctx: &Context<'_>,
    harness: &HarnessDir,
    a: &str,
    b: Option<&str>,
) -> Result<u8, Error> {
    let from = harness.resolve(a)?;
    let to = match b {
        Some(b) => harness.resolve(b)?,
        None => harness.active()?,
    };
    let (old, new) = (harness.tree(&from.id)?, harness.tree(&to.id)?);
    let summary = diff::summary(&old, &new);
    let unified = diff::unified(&old, &new, (&from.id, &to.id));
    if ctx.json {
        return json(&json!({"from": from.id, "to": to.id, "summary": summary, "diff": unified}));
    }
    print_raw(&format!("{unified}{summary}\n"))
}

fn commit_manual(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    harness: &HarnessDir,
    tree: &Tree,
    now: bool,
) -> Result<u8, Error> {
    let written = harness.write(tree, Source::Manual, None)?;
    activate(ctx, scope, &written, now)
}

/// `harness edit [PATH]`: the active tree is copied to a private scratch
/// directory, `$EDITOR` opens `PATH` in it, and the edited tree becomes a
/// manual version when it changed and validates.
fn edit(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    harness: &HarnessDir,
    path: &str,
    now: bool,
) -> Result<u8, Error> {
    let active = harness.active_id()?;
    let original = harness.tree(&active)?;
    let scratch = scope
        .dir()
        .join("cache")
        .join("harness-edit")
        .join(ulid::Ulid::generate().to_string());
    let io = |e: std::io::Error| Error::io(&codes::HARNESS_IO, &scratch, &e);
    original.write(&scratch).map_err(io)?;
    let file = scratch.join(pmt_config::harness::normalize(path).map_err(|why| {
        Error::new(
            &codes::HARNESS_INVALID,
            format!("invalid path '{path}': {why}"),
        )
    })?);
    let editor = ctx
        .launch
        .editor()
        .unwrap_or(crate::config_cmd::edit::DEFAULT_EDITOR);
    let edited = crate::config_cmd::edit::launch_editor(editor, &file)
        .and_then(|()| checked(ctx, scope, &scratch));
    let _ = std::fs::remove_dir_all(&scratch);
    let tree = edited?;
    if tree == original {
        return print_raw("no change; the harness stays as it was\n");
    }
    commit_manual(ctx, scope, harness, &tree, now)
}

/// Makes `written` active, records `harness.activated` and, with `--now`,
/// marks the open sessions for a rebuild.
fn activate(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    written: &VersionInfo,
    now: bool,
) -> Result<u8, Error> {
    let harness = HarnessDir::of(scope.dir());
    let from = harness.active_id()?;
    harness.activate(&written.id)?;
    record_activation(scope, &from, written);
    let marked = if now {
        let store = pmt_store::Store::open(scope)?;
        Some(crate::harness_host::now::activate(
            scope,
            &store,
            &written.id,
        )?)
    } else {
        None
    };
    if ctx.json {
        return json(&json!({
            "from": from,
            "to": written.id,
            "source": written.source.as_str(),
            "rebuilt_sessions": marked,
        }));
    }
    let when = match marked {
        Some(n) => format!("{n} open session(s) rebuild their prompt at their next turn"),
        None => "takes effect from the next session".to_owned(),
    };
    print_raw(&format!(
        "activated {} ({} of {from}); {when}\n",
        written.id,
        written.source.as_str()
    ))
}

/// Appends the `harness.activated` event (the trajectory grammar's line
/// shape, outside any session) to `<profile>/harness/activations.jsonl`.
pub(crate) fn record_activation(scope: &ProfileScope, from: &str, written: &VersionInfo) {
    let path = pmt_config::harness::dir(scope.dir()).join("activations.jsonl");
    let line = json!({
        "ts": jiff::Timestamp::now().to_string(),
        "type": "harness.activated",
        "payload": {"from": from, "to": written.id, "source": written.source.as_str()},
    });
    let appended = pmt_core::private_fs::append_options()
        .open(&path)
        .and_then(|mut file| {
            use std::io::Write as _;
            writeln!(file, "{line}")
        });
    if let Err(err) = appended {
        tracing::warn!(path = %path.display(), error = %err, "harness activation not recorded");
    }
    tracing::info!(from, to = %written.id, source = written.source.as_str(), "harness.activated");
}
