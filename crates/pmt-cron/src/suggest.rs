//! The suggested automations inbox (spec: scheduled-jobs, Suggested
//! automations inbox; skills, Blueprint skills never auto-schedule).
//!
//! Suggestions come from the curated starters (`catalog`), installed
//! skills with a `blueprint` block (`blueprint`), the background review
//! noticing a recurring request (`usage`) and newly connected accounts
//! (`integration`), each under a stable key. At most [`MAX_PENDING`] wait
//! at once (more are dropped with a DEBUG log); a dismissed key is latched
//! and never offered again; nothing becomes a job without an explicit
//! accept, which goes through the ordinary validated create path.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};
use pmt_store::Store;
use pmt_store::cron::suggestions::SuggestionRow;

use crate::blueprint::{Blueprint, Fill, builtin};
use crate::{scan, schedule};

/// Most suggestions pending at once.
pub const MAX_PENDING: usize = 5;

/// Where a suggestion comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The curated starters.
    Catalog,
    /// An installed skill's blueprint.
    Blueprint,
    /// The background review saw a recurring request.
    Usage,
    /// A newly connected account.
    Integration,
}

impl Source {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Blueprint => "blueprint",
            Self::Usage => "usage",
            Self::Integration => "integration",
        }
    }
}

/// A suggestion to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The stable key (a dismissed key is never offered again).
    pub key: String,
    /// Where it comes from.
    pub source: Source,
    /// What it offers (the job's name).
    pub title: String,
    /// The schedule, in the `prometheus cron` grammar (empty: a blueprint
    /// that still needs values).
    pub schedule: String,
    /// The prompt.
    pub prompt: String,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::CRON_INVALID_JOB, message)
}

impl Suggestion {
    /// A suggestion of `blueprint` with its defaults (schedule and prompt
    /// left empty when a required slot has no default: accepting it asks).
    #[must_use]
    pub fn of_blueprint(blueprint: &Blueprint, key: &str, source: Source) -> Self {
        let (schedule, prompt) = match blueprint.fill(&std::collections::BTreeMap::new()) {
            Ok(Fill::Ready(filled)) => (filled.schedule, filled.prompt),
            _ => (String::new(), String::new()),
        };
        Self {
            key: key.to_owned(),
            source,
            title: blueprint.title.clone(),
            schedule,
            prompt,
        }
    }

    /// Checks a proposed suggestion (the background review's): a key of
    /// `a-z0-9-_`, a schedule in the grammar, a prompt the safety scan
    /// accepts.
    ///
    /// # Errors
    ///
    /// `cron.invalid_job` / `cron.invalid_schedule` / `cron.prompt_rejected`.
    pub fn check(&self, now: Timestamp, zone: &TimeZone) -> Result<(), Error> {
        let key_ok = !self.key.is_empty()
            && self.key.len() <= 64
            && self.key.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b':')
            });
        if !key_ok {
            return Err(invalid(format!(
                "suggestion key '{}': use a-z, 0-9, '-', '_' and ':'",
                self.key
            )));
        }
        if self.title.trim().is_empty() || self.prompt.trim().is_empty() {
            return Err(invalid("a suggestion needs a title and a prompt"));
        }
        schedule::Schedule::parse(&self.schedule, now, zone)?;
        if let Some(rule) = scan::violation(&self.prompt) {
            return Err(Error::new(
                &codes::CRON_PROMPT_REJECTED,
                format!("prompt rejected: {rule}"),
            ));
        }
        Ok(())
    }
}

/// Offers `suggestion`; `false` when its key is known (pending, accepted
/// or dismissed) or [`MAX_PENDING`] are pending already.
///
/// # Errors
///
/// Store errors.
pub fn offer(store: &Store, suggestion: &Suggestion) -> Result<bool, Error> {
    let row = SuggestionRow {
        key: suggestion.key.clone(),
        source: suggestion.source.as_str().to_owned(),
        title: suggestion.title.clone(),
        schedule: suggestion.schedule.clone(),
        prompt: suggestion.prompt.clone(),
        ..SuggestionRow::default()
    };
    let added = store.cron_offer_suggestion(&row, MAX_PENDING)?;
    if !added {
        tracing::debug!(
            key = suggestion.key,
            source = suggestion.source.as_str(),
            "suggestion not offered (known key or the inbox is full)"
        );
    }
    Ok(added)
}

/// Offers the curated starters; how many were added.
///
/// # Errors
///
/// Store errors.
pub fn offer_catalog(store: &Store) -> Result<usize, Error> {
    let catalog = builtin::catalog();
    let mut added = 0;
    for key in builtin::STARTERS {
        if let Some(blueprint) = catalog.iter().find(|b| b.key == key) {
            let suggestion = Suggestion::of_blueprint(blueprint, key, Source::Catalog);
            added += usize::from(offer(store, &suggestion)?);
        }
    }
    Ok(added)
}

/// The pending suggestions, numbered from 1 in this order.
///
/// # Errors
///
/// Store errors.
pub fn pending(store: &Store) -> Result<Vec<SuggestionRow>, Error> {
    store.cron_suggestions("pending")
}

/// Pending suggestion number `n` (1-based).
///
/// # Errors
///
/// `cron.invalid_job` when there is no such number.
pub fn numbered(store: &Store, n: usize) -> Result<SuggestionRow, Error> {
    let pending = pending(store)?;
    n.checked_sub(1)
        .and_then(|i| pending.get(i).cloned())
        .ok_or_else(|| {
            invalid(format!(
                "no pending suggestion {n} ({} pending)",
                pending.len()
            ))
        })
}

/// Dismisses pending suggestion `n`: its key is never offered again.
///
/// # Errors
///
/// As [`numbered`], store errors.
pub fn dismiss(store: &Store, n: usize) -> Result<SuggestionRow, Error> {
    let row = numbered(store, n)?;
    store.cron_set_suggestion(&row.key, "dismissed")?;
    Ok(row)
}

/// Marks suggestion `key` accepted (its job was created).
///
/// # Errors
///
/// Store errors.
pub fn accepted(store: &Store, key: &str) -> Result<(), Error> {
    store.cron_set_suggestion(key, "accepted").map(|_| ())
}

/// The inbox as numbered lines, worded with `text`.
#[must_use]
pub fn render(text: &dyn pmt_core::text::Catalog, rows: &[SuggestionRow]) -> String {
    if rows.is_empty() {
        return text.text("slash.suggestions.none", &[]);
    }
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let when = if r.schedule.is_empty() {
                text.text("slash.suggestions.needs_values", &[])
            } else {
                r.schedule.clone()
            };
            format!(
                "{}. {} [{}] {}  ({})",
                i + 1,
                r.title,
                r.source,
                when,
                r.key
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
