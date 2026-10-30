//! Bodies of session slash commands whose behaviour belongs to the
//! configuration group. The typed slash-command registry (cli group)
//! registers them by name; each takes the session's [`ProfileScope`] and
//! returns the text to show.

use pmt_backup::snapshot;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

use crate::backup_cmd::{manual_snapshot, render_list};

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
