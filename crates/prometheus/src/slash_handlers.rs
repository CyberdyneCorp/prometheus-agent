//! Bodies of session slash commands whose behaviour belongs to the
//! configuration and session-storage groups. The typed slash-command
//! registry (cli group) registers them by name; each takes the session's
//! [`ProfileScope`] (and store) and returns the text to show.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use pmt_backup::snapshot;
use pmt_config::ProfileScope;
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_i18n::t;
use pmt_obs::budget::{self, Budgets, Place, Scope};
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
    let strings = strings(scope);
    Ok(if changed.is_empty() {
        t!(strings, "slash.secrets.unchanged")
    } else {
        t!(strings, "slash.secrets.changed", list = changed.join(", "))
    })
}

fn strings(scope: &ProfileScope) -> std::sync::Arc<pmt_i18n::Strings> {
    pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core)
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
    let strings = strings(scope);
    match (action, rest.as_slice()) {
        ("list", []) => {
            let list = snapshot::list(profile)?;
            Ok(if list.is_empty() {
                t!(strings, "slash.snapshot.none")
            } else {
                render_list(&list).trim_end().to_owned()
            })
        }
        ("create", label) => {
            let label = (!label.is_empty()).then(|| label.join(" "));
            let (taken, _) = manual_snapshot(profile, label.as_deref(), keep)?;
            Ok(t!(strings, "slash.snapshot.created", id = taken.id))
        }
        ("restore", [id]) => {
            let restored = snapshot::restore(profile, id)?;
            Ok(t!(
                strings,
                "slash.snapshot.restored",
                id = id,
                previous = restored.pre_restore.id
            ))
        }
        ("prune", []) => prune(&strings, profile, keep),
        ("prune", [n]) => match n.parse() {
            Ok(n) => prune(&strings, profile, n),
            Err(_) => Err(usage()),
        },
        _ => Err(usage()),
    }
}

fn prune(
    strings: &pmt_i18n::Strings,
    profile: &pmt_config::ProfileDir,
    keep: usize,
) -> Result<String, Error> {
    let removed = snapshot::prune(profile, keep)?;
    Ok(t!(strings, "slash.snapshot.removed", count = removed.len()))
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
    let traces = scope.config().moa.trace_dir(scope.dir());
    let options = ExportOptions {
        format,
        include_inactive: false,
        with_blobs: false,
        redactor: redactor.as_ref(),
        moa_traces: Some(&traces),
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
    Ok(t!(strings(scope), "slash.save.done", path = path.display()))
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

/// Usage of `/budget`.
pub const BUDGET_USAGE: &str = "/budget [set <turn|session|root|daily|monthly|unattended> <usd>]";

/// `/budget` (spec: observability, Budget warnings and overrides): without
/// arguments, every configured limit with its spend for this session;
/// `set <scope> <usd>` writes `budgets.<scope>_usd` to the profile's
/// config (validated, backed up) and returns the session's new budgets,
/// which apply to the very next model call (budgets are not part of the
/// system prompt). This is the user's command; agent tools are refused by
/// `pmt_obs::budget::guard`.
///
/// # Errors
///
/// `cli.usage` for malformed arguments; config write and store errors.
pub fn budget(
    scope: &ProfileScope,
    store: &Store,
    session: SessionId,
    current: &BudgetsConfig,
    args: &str,
) -> Result<(String, Option<BudgetsConfig>), Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] => {
            let text = budget_lines(scope, store, session, current)?
                .unwrap_or_else(|| t!(strings(scope), "slash.budget.none"));
            Ok((text, None))
        }
        ["set", name, usd] => {
            let budget_scope = Scope::parse(name).ok_or_else(budget_usage)?;
            let value: f64 = usd
                .trim_start_matches('$')
                .parse()
                .map_err(|_| budget_usage())?;
            let key = format!("budgets.{}", budget_scope.usd_key());
            scope
                .profile()
                .config_file()
                .set(&key, &value.to_string())?;
            let mut updated = current.clone();
            budget::set_usd(&mut updated, budget_scope, value);
            let text = t!(strings(scope), "slash.budget.set", key = key, value = value);
            Ok((text, Some(updated)))
        }
        _ => Err(budget_usage()),
    }
}

/// Every configured limit of `session` with its spend; `None` when no
/// budget is set.
///
/// # Errors
///
/// Store errors, an invalid profile time zone.
pub fn budget_lines(
    scope: &ProfileScope,
    store: &Store,
    session: SessionId,
    current: &BudgetsConfig,
) -> Result<Option<String>, Error> {
    let zone = pmt_obs::time::profile_zone(&scope.config().cron)?;
    let place = Place::of(store, session, None)?;
    let rows = Budgets::new(zone).status(store, current, &place, false, jiff::Timestamp::now())?;
    if rows.is_empty() {
        return Ok(None);
    }
    let strings = strings(scope);
    let percent = format!("{:.0}", current.warn_ratio * 100.0);
    let mut text = t!(strings, "slash.budget.heading", percent = percent);
    for (limit, spent) in rows {
        let row = t!(
            strings,
            "slash.budget.row",
            scope = format!("{:<10}", limit.scope.as_str()),
            spent = budget::format_amount(limit.unit, spent),
            limit = budget::format_amount(limit.unit, limit.limit)
        );
        let _ = write!(text, "\n  {row}");
    }
    Ok(Some(text))
}

fn budget_usage() -> Error {
    Error::new(&codes::CLI_USAGE, format!("usage: {BUDGET_USAGE}"))
}

/// `/usage` (spec: agent-turn-loop, Cache-hit reporting and break
/// detection; observability): the session's own model calls, tokens by
/// class, cost and prompt-cache hit rate (cache-read tokens over all input
/// tokens).
///
/// # Errors
///
/// `session.not_found`, store errors.
pub fn session_usage(
    strings: &pmt_i18n::Strings,
    store: &Store,
    session: SessionId,
) -> Result<String, Error> {
    let totals = store.usage_totals(pmt_store::UsageScope::Session(session))?;
    let t = &totals.tokens;
    let hit_rate = t
        .cache_hit_rate()
        .map_or_else(|| "—".to_owned(), |rate| format!("{:.0}%", rate * 100.0));
    let mut text = t!(
        strings,
        "slash.usage.session_totals",
        calls = totals.calls,
        input = t.input,
        cache_read = t.cache_read,
        cache_write = t.cache_write,
        output = t.output,
        cost = pmt_obs::usage::format_total_cost(&totals),
        rate = hit_rate
    );
    if let Some(billing) = billing_lines(strings, store, session)? {
        text.push('\n');
        text.push_str(&billing);
    }
    Ok(text)
}

/// The billing source of `session`'s latest model call and, for a
/// subscription, its plan and quota (spec: model-providers, Billing source
/// transparency; Subscription plan and quota visibility).
///
/// # Errors
///
/// Store errors.
pub fn billing_lines(
    strings: &pmt_i18n::Strings,
    store: &Store,
    session: SessionId,
) -> Result<Option<String>, Error> {
    let Some(last) = store.usage_events(session)?.pop() else {
        return Ok(None);
    };
    let mut text = t!(
        strings,
        "slash.billing.source",
        source = last.billing_source
    );
    if let Some((reported, quota)) =
        pmt_providers::quota::for_session(store, &last.provider, &last.billing_source)
    {
        let line = match (quota, reported) {
            (Some(quota), _) => t!(strings, "slash.billing.quota", quota = quota.describe()),
            (None, true) => t!(strings, "slash.billing.not_fetched"),
            (None, false) => pmt_providers::quota::NOT_REPORTED.to_owned(),
        };
        text.push('\n');
        text.push_str(&line);
    }
    Ok(Some(text))
}

/// Usage of `/personality` (surfaces cli, tui, desktop, gateway).
pub const PERSONALITY_USAGE: &str = "/personality [<name>|none|default|neutral] [--now]";

/// `/personality` (spec: agent-turn-loop, Personality overlays): without
/// arguments, `none` and every personality with the active one marked;
/// with a name, writes `agent.personality` (`none` for `none`, `default`
/// or `neutral`) so the next session's prompt carries the overlay. With
/// `--now` the second value is the selection to apply to the running
/// session at once (the caller passes it to
/// `pmt_agent::turn::Agent::apply_personality_now`, a recorded cache
/// break); otherwise the current prompt is left untouched.
///
/// # Errors
///
/// `cli.usage` for malformed arguments, `personality.unknown` (exit 2)
/// listing the valid names, config write errors.
pub fn personality(scope: &ProfileScope, args: &str) -> Result<(String, Option<String>), Error> {
    use pmt_agent::prompt::personality::{canonical, listing};
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let now = words.contains(&"--now");
    words.retain(|w| *w != "--now");
    let agent = &scope.config().agent;
    match (words.as_slice(), now) {
        ([], false) => Ok((listing(&*strings(scope), agent), None)),
        ([name], _) => {
            let selected = canonical(agent, name)?;
            scope
                .profile()
                .config_file()
                .set("agent.personality", &selected)?;
            let strings = strings(scope);
            if now {
                let text = t!(strings, "slash.personality.now", name = selected);
                return Ok((text, Some(selected)));
            }
            Ok((t!(strings, "slash.personality.next", name = selected), None))
        }
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            format!("usage: {PERSONALITY_USAGE}"),
        )),
    }
}
