//! Finding suites (spec: replay-and-evaluation, Eval suite format): a
//! path given on the command line, else a suite under
//! `<profile>/evals/<name>/`, else a suite bundled with the binary
//! (`smoke`), which is written to `<profile>/evals/.bundled/<name>/` so
//! that runs and judges read it from disk like any other suite.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_eval::suite::{self, Suite};
use serde_json::json;

use crate::context::Context;

/// The directory of bundled suites under a profile.
const BUNDLED: &str = ".bundled";

/// `<profile>/evals`.
#[must_use]
pub fn evals_dir(scope: &ProfileScope) -> PathBuf {
    scope.dir().join("evals")
}

/// Writes the bundled suite `name` under the profile (refreshed when this
/// build's files differ); `None` when no suite of that name is bundled.
fn bundled(scope: &ProfileScope, name: &str) -> Result<Option<PathBuf>, Error> {
    if name != "smoke" {
        return Ok(None);
    }
    let dir = evals_dir(scope).join(BUNDLED).join(name);
    for file in crate::smoke_suite::FILES {
        let target = dir.join(file.path);
        if std::fs::read_to_string(&target).is_ok_and(|t| t == file.text) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(&codes::EVAL_IO, parent, &e))?;
        }
        std::fs::write(&target, file.text).map_err(|e| Error::io(&codes::EVAL_IO, &target, &e))?;
    }
    Ok(Some(dir))
}

/// The directory `spec` names.
///
/// # Errors
///
/// `eval.suite_not_found` (66); `eval.io` writing a bundled suite.
pub fn locate(scope: &ProfileScope, spec: &str) -> Result<PathBuf, Error> {
    let path = Path::new(spec);
    if path.join("suite.toml").is_file() {
        return Ok(path.to_path_buf());
    }
    let named = evals_dir(scope).join(spec);
    if suite::is_relative_inside(spec) && named.join("suite.toml").is_file() {
        return Ok(named);
    }
    if let Some(dir) = bundled(scope, spec)? {
        return Ok(dir);
    }
    Err(Error::new(
        &codes::EVAL_SUITE_NOT_FOUND,
        format!(
            "no eval suite '{spec}': not a directory with a suite.toml, not under {}, not bundled",
            evals_dir(scope).display()
        ),
    ))
}

/// Loads the suite `spec` names.
///
/// # Errors
///
/// As [`locate`] and [`Suite::load`].
pub fn load(scope: &ProfileScope, spec: &str) -> Result<Suite, Error> {
    let dir = locate(scope, spec)?;
    // Absolute, so grader paths compare against what sessions see.
    let dir = dir.canonicalize().unwrap_or(dir);
    Suite::load(&dir)
}

/// `prometheus eval validate`.
///
/// # Errors
///
/// `eval.invalid_suite` (65) naming the first invalid file.
pub fn validate(spec: &str, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let suite = load(&scope, spec)?;
    if ctx.json {
        return crate::output::document(
            "eval validate",
            &json!({"suite": suite.meta.name, "version": suite.meta.version, "tasks": suite.tasks.len()}),
        );
    }
    crate::output::print(&format!(
        "suite {} {} is valid ({} tasks)",
        suite.meta.name,
        suite.meta.version,
        suite.tasks.len()
    ))
}

/// `prometheus eval list`.
///
/// # Errors
///
/// Profile errors.
pub fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut rows = Vec::new();
    for dir in suite::discover(&evals_dir(&scope)) {
        let row = match Suite::load(&dir) {
            Ok(s) => {
                json!({"name": s.meta.name, "version": s.meta.version, "tasks": s.tasks.len(), "path": dir})
            }
            Err(err) => {
                json!({"name": dir.file_name().map(|n| n.to_string_lossy().into_owned()), "error": err.message(), "path": dir})
            }
        };
        rows.push(row);
    }
    rows.push(
        json!({"name": "smoke", "bundled": true, "tasks": crate::smoke_suite::task_ids().len()}),
    );
    if ctx.json {
        return crate::output::document("eval list", &json!({"suites": rows}));
    }
    let mut text = String::new();
    for row in &rows {
        let name = row["name"].as_str().unwrap_or("?");
        let detail = match (&row["error"], row["bundled"].as_bool()) {
            (serde_json::Value::String(e), _) => format!("invalid: {e}"),
            (_, Some(true)) => format!("bundled, {} tasks", row["tasks"]),
            _ => format!("{} tasks", row["tasks"]),
        };
        let _ = writeln!(text, "{name}\t{detail}");
    }
    crate::output::print_raw(&text)
}
