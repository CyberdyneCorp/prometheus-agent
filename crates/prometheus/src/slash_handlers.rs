//! Bodies of session slash commands whose behaviour belongs to the
//! configuration and session-storage groups. The typed slash-command
//! registry (cli group) registers them by name; each takes the session's
//! [`ProfileScope`] (and store) and returns the text to show.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_backup::snapshot;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;

use crate::backup_cmd::{manual_snapshot, render_list};
use crate::cli::ExportFormat;
use crate::session_export::{self, ExportOptions};

/// Usage of `/snapshot` (alias `/snap`; surfaces cli, tui, desktop).
pub const SNAPSHOT_USAGE: &str = "/snapshot [create [label]|list|restore <id>|prune [n]]";

/// `/reload-secrets` (spec: configuration-and-profiles, Secrets are
/// resolved per use): drops the session profile's cached secret values so
/// the next request reads the store, and names (never shows) the secrets
/// whose value changed since their last use.
///
/// # Errors
///
/// Secrets store errors.
pub fn reload_secrets(scope: &ProfileScope) -> Result<String, Error> {
    let changed = scope.secrets().reload()?;
    Ok(if changed.is_empty() {
        "secrets reloaded; no value changed".to_owned()
    } else {
        format!("secrets reloaded; changed: {}", changed.join(", "))
    })
}

/// `/snapshot [create [label]|list|restore <id>|prune [n]]` (spec:
/// configuration-and-profiles, State snapshots) on the session's profile.
/// It runs between turns, so restoring is not blocked by the session's own
/// runtime record; the caller reopens the profile's store afterwards.
///
/// # Errors
///
/// `cli.usage` for malformed arguments; snapshot errors.
pub fn snapshot(scope: &ProfileScope, args: &str) -> Result<String, Error> {
    let profile = scope.profile();
    let keep = usize::try_from(scope.config().snapshots.keep).unwrap_or(usize::MAX);
    let mut words = args.split_whitespace();
    let action = words.next().unwrap_or("list");
    let rest: Vec<&str> = words.collect();
    match (action, rest.as_slice()) {
        ("list", []) => {
            let list = snapshot::list(profile)?;
            Ok(if list.is_empty() {
                "no snapshots".to_owned()
            } else {
                render_list(&list).trim_end().to_owned()
            })
        }
        ("create", label) => {
            let label = (!label.is_empty()).then(|| label.join(" "));
            let (taken, _) = manual_snapshot(profile, label.as_deref(), keep)?;
            Ok(format!("created {}", taken.id))
        }
        ("restore", [id]) => {
            let restored = snapshot::restore(profile, id)?;
            Ok(format!(
                "restored {id}; the replaced state is snapshot {}",
                restored.pre_restore.id
            ))
        }
        ("prune", []) => prune(profile, keep),
        ("prune", [n]) => match n.parse() {
            Ok(n) => prune(profile, n),
            Err(_) => Err(usage()),
        },
        _ => Err(usage()),
    }
}

fn prune(profile: &pmt_config::ProfileDir, keep: usize) -> Result<String, Error> {
    let removed = snapshot::prune(profile, keep)?;
    Ok(format!("removed {} snapshot(s)", removed.len()))
}

fn usage() -> Error {
    Error::new(&codes::CLI_USAGE, format!("usage: {SNAPSHOT_USAGE}"))
}

/// Usage of `/save`.
pub const SAVE_USAGE: &str = "/save [json|md|html] [file] [redact]";

/// `/save [json|md|html] [file] [redact]` (spec: session-storage, Export
/// formats and save command): writes the current session (active
/// messages) to `file` (relative to `cwd`) or to
/// `<cwd>/<title-slug>-<YYYYMMDD-HHMM>.<ext>`; `redact` scrubs secrets and
/// credential-shaped text. `json` is the portable JSONL export. A bare
/// `/save` returns the usage.
///
/// # Errors
///
/// `cli.usage` for malformed arguments, `output.io` when the file exists
/// or cannot be written, store errors.
pub fn save(
    scope: &ProfileScope,
    store: &Store,
    session: SessionId,
    args: &str,
    cwd: &Path,
) -> Result<String, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    if words.is_empty() {
        return Ok(format!("usage: {SAVE_USAGE}"));
    }
    let redact = words.last() == Some(&"redact");
    if redact {
        words.pop();
    }
    let format = match words.first() {
        Some(&"json") => Some(ExportFormat::Jsonl),
        Some(&"md") => Some(ExportFormat::Md),
        Some(&"html") => Some(ExportFormat::Html),
        _ => None,
    };
    if format.is_some() {
        words.remove(0);
    }
    let format = format.unwrap_or(ExportFormat::Jsonl);
    let path = match words.as_slice() {
        [] => default_save_path(store, session, format, cwd)?,
        [file] => cwd.join(file),
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("usage: {SAVE_USAGE}"),
            ));
        }
    };
    let redactor = redact
        .then(|| crate::sessions_cmd::redactor(scope))
        .transpose()?;
    let options = ExportOptions {
        format,
        include_inactive: false,
        redactor: redactor.as_ref(),
    };
    let io_error = |e: &std::io::Error| {
        Error::new(
            &codes::OUTPUT_IO,
            format!("cannot write {}: {e}", path.display()),
        )
    };
    let mut file = pmt_core::private_fs::new_file_options()
        .open(&path)
        .map_err(|e| io_error(&e))?;
    session_export::export(store, &[session], options, &mut file)?;
    file.flush().map_err(|e| io_error(&e))?;
    Ok(format!("saved {}", path.display()))
}

fn default_save_path(
    store: &Store,
    session: SessionId,
    format: ExportFormat,
    cwd: &Path,
) -> Result<PathBuf, Error> {
    let title = store.session(session)?.title;
    let slug = slug(title.as_deref().unwrap_or(&session.to_string()));
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M");
    Ok(cwd.join(format!(
        "{slug}-{stamp}.{}",
        session_export::extension(format)
    )))
}

/// Lowercase ASCII letters and digits, other runs as one `-`.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug.chars().take(60).collect()
    }
}
