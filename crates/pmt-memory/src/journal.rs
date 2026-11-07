//! The user's view of the revision journal (spec: memory, Memory revision
//! journal; Reviewable memory changes per session): `memory log`, `memory
//! diff`, `memory revert`, the session-end summary and `/memory review`.
//!
//! Revisions are written by the store with every committed batch; this
//! module reads them, renders them and reverts them. A revert applies the
//! inverse change as a new revision with `origin = user`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_store::{Author, MemoryEntry, MemoryOrigin, Operation, Revision, RevisionFilter, Store};
use similar::TextDiff;

/// Where a log or diff starts: after a revision, or since a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// Revisions after this one.
    Rev(i64),
    /// Revisions at or after this time.
    Time(Timestamp),
}

impl Since {
    /// Parses `--since`: a revision number, or a duration like `2h`, `3d`,
    /// `45m`, `1w` back from `now`.
    ///
    /// # Errors
    ///
    /// `cli.usage` for anything else.
    pub fn parse(text: &str, now: Timestamp) -> Result<Self, Error> {
        let text = text.trim();
        if let Ok(rev) = text.parse::<i64>() {
            return Ok(Self::Rev(rev));
        }
        let usage = || {
            Error::new(
                &codes::CLI_USAGE,
                format!(
                    "--since expects a revision number or a duration like 2h, 3d or 1w, not '{text}'"
                ),
            )
        };
        let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(usage)?;
        let (count, unit) = text.split_at(split);
        let count: i64 = count.parse().map_err(|_| usage())?;
        let secs = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 604_800,
            _ => return Err(usage()),
        };
        let back = jiff::SignedDuration::from_secs(count.saturating_mul(secs));
        Ok(Self::Time(now.checked_sub(back).map_err(|_| usage())?))
    }

    /// The store filter for this start.
    #[must_use]
    pub fn filter(self) -> RevisionFilter {
        match self {
            Self::Rev(rev) => RevisionFilter {
                after_rev: Some(rev),
                ..RevisionFilter::default()
            },
            Self::Time(at) => RevisionFilter {
                since: Some(at),
                ..RevisionFilter::default()
            },
        }
    }
}

/// One `memory log` line: `42 2026-11-07T10:00:00Z patch m_x agent ses_y: Lives in Porto.`
#[must_use]
pub fn log_line(r: &Revision) -> String {
    let content = r
        .after
        .as_ref()
        .or(r.before.as_ref())
        .map(|e| e.content.as_str())
        .unwrap_or_default();
    let session = r
        .session_id
        .as_deref()
        .map(|s| format!(" {s}"))
        .unwrap_or_default();
    format!(
        "{} {} {} {} {}{session}: {}",
        r.rev,
        r.at.strftime("%Y-%m-%dT%H:%M:%SZ"),
        r.operation.as_str(),
        r.entry_id,
        r.origin,
        one_line(content)
    )
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 80 {
        let cut: String = flat.chars().take(79).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// A unified diff per entry over `revisions`: each entry's content before
/// its first revision against after its last.
#[must_use]
pub fn diff(revisions: &[Revision]) -> String {
    let mut spans: BTreeMap<&str, (&Revision, &Revision)> = BTreeMap::new();
    for r in revisions {
        spans
            .entry(r.entry_id.as_str())
            .and_modify(|span| span.1 = r)
            .or_insert((r, r));
    }
    let mut out = String::new();
    for (id, (first, last)) in spans {
        let old = side(id, first.before.as_ref());
        let new = side(id, last.after.as_ref());
        if old.1 == new.1 {
            continue;
        }
        let _ = write!(
            out,
            "{}",
            TextDiff::from_lines(&old.1, &new.1)
                .unified_diff()
                .header(&format!("a/{}", old.0), &format!("b/{}", new.0))
        );
    }
    out
}

/// One side of an entry's diff: its name (`/dev/null` when absent) and
/// its text as lines.
fn side(id: &str, entry: Option<&MemoryEntry>) -> (String, String) {
    entry.map_or_else(
        || ("/dev/null".to_owned(), String::new()),
        |e| (id.to_owned(), format!("{}\n", e.content)),
    )
}

/// Reverts revision `rev`: the entry is put back as it was before it
/// (removed, for an add), recorded as a new revision with `origin = user`.
/// Returns the new revision's number.
///
/// # Errors
///
/// `memory.not_found` for an unknown revision; store errors.
pub fn revert(store: &Store, rev: i64) -> Result<i64, Error> {
    let revision = store.memory_revision(rev)?.ok_or_else(|| {
        Error::new(
            &codes::MEMORY_NOT_FOUND,
            format!("no memory revision {rev}"),
        )
        .with_hint("prometheus memory log")
    })?;
    store.memory_batch(None, |b| {
        b.author = Some(Author::user());
        b.entries.retain(|e| e.id != revision.entry_id);
        if let Some(before) = &revision.before {
            let mut restored = before.clone();
            restored.updated_at = Timestamp::now();
            b.entries.push(restored);
        }
        Ok(())
    })?;
    let newest = store.memory_revisions(&RevisionFilter {
        after_rev: Some(rev),
        ..RevisionFilter::default()
    })?;
    store.mark_memory_reviewed(&[rev])?;
    Ok(newest.last().map_or(rev, |r| r.rev))
}

/// Counts of one session's unreviewed agent changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Entries added.
    pub added: usize,
    /// Entries patched (content or other fields).
    pub patched: usize,
    /// Entries removed.
    pub removed: usize,
}

impl Counts {
    /// No change at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.added + self.patched + self.removed == 0
    }

    /// `Memory: +2 ~1 -0 (review with /memory review)`.
    #[must_use]
    pub fn line(self) -> String {
        format!(
            "Memory: +{} ~{} -{} (review with /memory review)",
            self.added, self.patched, self.removed
        )
    }
}

const fn by_the_agent(origin: MemoryOrigin) -> bool {
    matches!(origin, MemoryOrigin::Agent | MemoryOrigin::BackgroundReview)
}

/// The unreviewed agent changes of `session`, oldest first.
///
/// # Errors
///
/// Store errors.
pub fn session_changes(store: &Store, session: &str) -> Result<Vec<Revision>, Error> {
    Ok(store
        .memory_revisions(&RevisionFilter {
            session: Some(session.to_owned()),
            unreviewed: true,
            ..RevisionFilter::default()
        })?
        .into_iter()
        .filter(|r| by_the_agent(r.origin))
        .collect())
}

/// The counts behind the session summary.
#[must_use]
pub fn counts(changes: &[Revision]) -> Counts {
    let mut counts = Counts::default();
    for r in changes {
        match r.operation {
            Operation::Add => counts.added += 1,
            Operation::Patch | Operation::Update => counts.patched += 1,
            Operation::Remove => counts.removed += 1,
        }
    }
    counts
}

/// The summary shown when `session` ends or is resumed: `None` under
/// `review_mode = "off"` or when the agent changed nothing (unreviewed).
///
/// # Errors
///
/// Store errors.
pub fn summary(
    store: &Store,
    mode: pmt_config::schema::MemoryReviewMode,
    session: &str,
) -> Result<Option<String>, Error> {
    if mode == pmt_config::schema::MemoryReviewMode::Off {
        return Ok(None);
    }
    let counts = counts(&session_changes(store, session)?);
    Ok((!counts.is_empty()).then(|| counts.line()))
}

/// `/memory review [keep|revert <rev>|all]` for `session`.
///
/// # Errors
///
/// `memory.not_found` for a revision that is not one of this session's
/// unreviewed changes; store errors.
pub fn review(store: &Store, session: &str, args: &[&str]) -> Result<String, Error> {
    let changes = session_changes(store, session)?;
    let pick = |which: &str| -> Result<Vec<i64>, Error> {
        if which == "all" {
            return Ok(changes.iter().map(|r| r.rev).collect());
        }
        let rev: i64 = which
            .parse()
            .ok()
            .filter(|rev| changes.iter().any(|r| r.rev == *rev))
            .ok_or_else(|| {
                Error::new(
                    &codes::MEMORY_NOT_FOUND,
                    format!("'{which}' is not a memory change of this session awaiting review"),
                )
                .with_hint("/memory review")
            })?;
        Ok(vec![rev])
    };
    match args {
        [] => Ok(listing(&changes)),
        ["keep", which] => {
            let revs = pick(which)?;
            store.mark_memory_reviewed(&revs)?;
            Ok(format!("Kept {} change(s).", revs.len()))
        }
        ["revert", which] => {
            let revs = pick(which)?;
            for rev in revs.iter().rev() {
                revert(store, *rev)?;
            }
            Ok(format!("Reverted {} change(s).", revs.len()))
        }
        _ => Ok(REVIEW_USAGE.to_owned()),
    }
}

/// The usage of `/memory review`.
pub const REVIEW_USAGE: &str = "usage: /memory review [keep <rev>|all | revert <rev>|all]";

fn listing(changes: &[Revision]) -> String {
    if changes.is_empty() {
        return "No memory changes of this session to review.".to_owned();
    }
    let mut text = String::new();
    for r in changes {
        let _ = writeln!(
            text,
            "rev {} ({} {}, {}):",
            r.rev,
            r.operation.as_str(),
            r.entry_id,
            r.origin
        );
        for line in diff(std::slice::from_ref(r)).lines().skip(2) {
            let _ = writeln!(text, "  {line}");
        }
        let _ = writeln!(
            text,
            "  [keep: /memory review keep {0}] [revert: /memory review revert {0}]",
            r.rev
        );
    }
    text.push_str("Keep or revert every change with /memory review keep all | revert all.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_revision_or_a_duration() {
        let now: Timestamp = "2026-11-07T12:00:00Z".parse().unwrap();
        assert_eq!(Since::parse("42", now).unwrap(), Since::Rev(42));
        assert_eq!(
            Since::parse("2h", now).unwrap(),
            Since::Time("2026-11-07T10:00:00Z".parse().unwrap())
        );
        assert!(Since::parse("soon", now).is_err());
    }
}
