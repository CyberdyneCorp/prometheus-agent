//! The low-disk prune prompt and `/autoprune` (spec: session-storage,
//! Low-disk prune prompt).
//!
//! At an interactive start, at most once a day, when the volume holding
//! `state.db` has less than `sessions.low_disk_bytes` free and pruning is
//! off (`sessions.retention_secs = 0`), the user is offered auto-prune
//! once: `on` sets a 90-day retention, `off` never asks again and `later`
//! asks again after a week. The answers live in the store, never in
//! `config.toml`.

use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_store::Store;

/// The retention `on` sets: 90 days.
pub const ON_RETENTION_SECS: u64 = 7_776_000;
/// The question, in the profile's language.
#[must_use]
pub fn question(scope: &ProfileScope) -> String {
    pmt_i18n::t!(strings(scope), "cli.autoprune_question")
}

fn strings(scope: &ProfileScope) -> std::sync::Arc<pmt_i18n::Strings> {
    pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core)
}

const ANSWER: &str = "autoprune.answer";
const CHECKED: &str = "autoprune.checked_at";
const DAY: SignedDuration = SignedDuration::from_hours(24);
const WEEK: SignedDuration = SignedDuration::from_hours(24 * 7);

/// An answer to the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Turn auto-prune on.
    On,
    /// Never ask again.
    Off,
    /// Ask again in a week.
    Later,
}

impl Answer {
    /// Parses `on`, `off` or `later`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" | "y" | "yes" => Some(Self::On),
            "off" | "never" => Some(Self::Off),
            "later" | "" => Some(Self::Later),
            _ => None,
        }
    }
}

/// Free bytes on the volume holding `path` (the mount point that is its
/// longest prefix).
#[must_use]
pub fn free_space(path: &Path) -> Option<u64> {
    let path = std::fs::canonicalize(path).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(sysinfo::Disk::available_space)
}

fn stamp(value: Option<String>) -> Option<Timestamp> {
    value
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
}

/// Whether the prompt is due now; a `true` records today's check.
///
/// # Errors
///
/// Store errors.
pub fn due(scope: &ProfileScope, store: &Store, now: Timestamp) -> Result<bool, Error> {
    let sessions = &scope.config().sessions;
    if sessions.retention_secs != 0 {
        return Ok(false);
    }
    match store.pref(ANSWER)?.as_deref() {
        Some("off") => return Ok(false),
        Some(later) => {
            if let Some(at) = stamp(later.strip_prefix("later:").map(str::to_owned))
                && now.duration_since(at) < WEEK
            {
                return Ok(false);
            }
        }
        None => {}
    }
    if let Some(at) = stamp(store.pref(CHECKED)?)
        && now.duration_since(at) < DAY
    {
        return Ok(false);
    }
    store.set_pref(CHECKED, &now.as_millisecond().to_string())?;
    Ok(free_space(scope.dir()).is_some_and(|free| free < sessions.low_disk_bytes))
}

/// Applies `answer`; the confirmation line.
///
/// # Errors
///
/// Config write errors (`on`), store errors.
pub fn apply(
    scope: &ProfileScope,
    store: &Store,
    answer: Answer,
    now: Timestamp,
) -> Result<String, Error> {
    match answer {
        Answer::On => {
            if scope.config().sessions.retention_secs == 0 {
                scope
                    .profile()
                    .config_file()
                    .set("sessions.retention_secs", &ON_RETENTION_SECS.to_string())?;
            }
            store.set_pref(ANSWER, "on")?;
            Ok(pmt_i18n::t!(strings(scope), "cli.autoprune_on"))
        }
        Answer::Off => {
            store.set_pref(ANSWER, "off")?;
            Ok(pmt_i18n::t!(strings(scope), "cli.autoprune_off"))
        }
        Answer::Later => {
            store.set_pref(ANSWER, &format!("later:{}", now.as_millisecond()))?;
            Ok(pmt_i18n::t!(strings(scope), "cli.autoprune_later"))
        }
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{} MiB", bytes / 1_048_576)
}

/// `/autoprune status`.
fn status(scope: &ProfileScope, store: &Store) -> String {
    let retention = scope.config().sessions.retention_secs;
    let database = store.location().database();
    let size: u64 = ["", "-wal"]
        .iter()
        .filter_map(|suffix| {
            let mut name = database.as_os_str().to_owned();
            name.push(suffix);
            std::fs::metadata(name).ok().map(|m| m.len())
        })
        .sum();
    let strings = strings(scope);
    let free =
        free_space(scope.dir()).map_or_else(|| pmt_i18n::t!(strings, "cli.unknown"), megabytes);
    let pruning = if retention == 0 {
        pmt_i18n::t!(strings, "cli.off")
    } else {
        pmt_i18n::t!(strings, "cli.autoprune_days", days = retention / 86_400)
    };
    pmt_i18n::t!(
        strings,
        "cli.autoprune_status",
        pruning = pruning,
        retention = retention,
        size = megabytes(size),
        free = free
    )
}

/// `/autoprune [on|off|later|status]`.
///
/// # Errors
///
/// `cli.usage` for another argument; config and store errors.
pub fn command(scope: &ProfileScope, store: &Store, args: &str) -> Result<String, Error> {
    let args = args.trim();
    if args.is_empty() || args == "status" {
        return Ok(status(scope, store));
    }
    let answer = match args {
        "on" => Answer::On,
        "off" => Answer::Off,
        "later" => Answer::Later,
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "usage: /autoprune [on|off|later|status]",
            ));
        }
    };
    apply(scope, store, answer, Timestamp::now())
}
