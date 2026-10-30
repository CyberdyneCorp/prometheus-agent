//! Session titles: sanitising, provenance, `<base> #<n>` lineage numbering
//! and resolution of a session reference (spec: session-storage, Session
//! titles with provenance; Title lineage numbering and resolution).

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::sql;
use crate::model::{LineageKind, TitleSource};
use crate::sessions::{self, Session};
use crate::store::Store;

/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 100;

/// Shortest id prefix accepted as a session reference.
pub const MIN_PREFIX_CHARS: usize = 8;

/// The outcome of a title write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleWrite {
    /// The title was stored.
    Written,
    /// An automatic title lost to an equal or higher provenance.
    Kept,
}

/// Strips control, zero-width and bidi-control characters and collapses
/// whitespace; an empty result is no title. (Rust strings cannot hold lone
/// surrogates, so input decoding already removed those.)
///
/// # Errors
///
/// `store.title_too_long`: `Title too long (<n> chars, max 100)`.
pub fn sanitize(raw: &str) -> Result<Option<String>, Error> {
    let kept: String = raw.chars().filter(|c| !is_stripped(*c)).collect();
    let title = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = title.chars().count();
    if chars > MAX_TITLE_CHARS {
        return Err(Error::new(
            &codes::STORE_TITLE_TOO_LONG,
            format!("Title too long ({chars} chars, max {MAX_TITLE_CHARS})"),
        ));
    }
    Ok((!title.is_empty()).then_some(title))
}

fn is_stripped(c: char) -> bool {
    // Whitespace controls (tab, newline) collapse to spaces instead.
    (c.is_control() && !c.is_whitespace())
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{061C}'
                | '\u{FEFF}'
        )
}

/// Splits `Refactor #3` into (`Refactor`, 3); an unnumbered title is #1.
fn split_number(title: &str) -> (&str, u32) {
    title
        .rsplit_once(" #")
        .and_then(|(base, n)| {
            let valid =
                !base.is_empty() && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit());
            valid.then(|| n.parse().ok().map(|n| (base, n))).flatten()
        })
        .unwrap_or((title, 1))
}

/// The titles of `base`'s family (`base` itself and `base #<n>`) with
/// their numbers.
fn family(conn: &Connection, base: &str) -> Result<Vec<(u32, String)>, Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT title FROM sessions WHERE title = ?1 \
             OR substr(title, 1, length(?1) + 2) = ?1 || ' #'",
        )
        .map_err(sql)?;
    let titles = stmt
        .query_map([base], |row| row.get::<_, String>(0))
        .map_err(sql)?;
    let mut members = Vec::new();
    for title in titles {
        let title = title.map_err(sql)?;
        let (stem, n) = split_number(&title);
        if stem == base {
            members.push((n, title));
        }
    }
    Ok(members)
}

/// The next free `<base> #<n>` after every existing family member.
fn next_numbered(conn: &Connection, title: &str) -> Result<String, Error> {
    let (base, _) = split_number(title);
    let next = family(conn, base)?
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(1)
        + 1;
    let suffix = format!(" #{next}");
    let room = MAX_TITLE_CHARS - suffix.chars().count();
    let base: String = base.chars().take(room).collect();
    Ok(format!("{}{suffix}", base.trim_end()))
}

/// The title a new child of `parent` inherits: compression continuations
/// and forks of a titled session are numbered, other children untitled.
pub(crate) fn inherited(
    conn: &Connection,
    parent: &Session,
    kind: LineageKind,
) -> Result<(Option<String>, Option<TitleSource>), Error> {
    match (&parent.title, kind) {
        (Some(title), LineageKind::Compression | LineageKind::Fork) => {
            Ok((Some(next_numbered(conn, title)?), parent.title_source))
        }
        _ => Ok((None, None)),
    }
}

/// Fails when another session (other than `except`) has `title`.
pub(crate) fn ensure_unused(
    conn: &Connection,
    title: &str,
    except: Option<SessionId>,
) -> Result<(), Error> {
    let holder: Option<String> = conn
        .query_row(
            "SELECT id FROM sessions WHERE title = ?1 AND id IS NOT ?2",
            params![title, except.map(|id| id.to_string())],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    match holder {
        None => Ok(()),
        Some(id) => Err(Error::new(
            &codes::STORE_TITLE_IN_USE,
            format!("Title '{title}' is already in use by session {id}"),
        )),
    }
}

impl Store {
    /// Sets a session's title. A `user` write always lands (`None`
    /// clears); an automatic (`derived`/`llm`) write lands only on an
    /// untitled session or one whose title has strictly lower provenance,
    /// checked and written in one transaction.
    ///
    /// # Errors
    ///
    /// `store.title_too_long`, `store.title_in_use`, `session.not_found`,
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn set_title(
        &self,
        id: SessionId,
        title: Option<&str>,
        source: TitleSource,
    ) -> Result<TitleWrite, Error> {
        let title = title.map(sanitize).transpose()?.flatten();
        if title.is_none() && source != TitleSource::User {
            return Ok(TitleWrite::Kept);
        }
        self.write(|tx| {
            let current = sessions::load(tx, id)?.ok_or_else(|| sessions::not_found(id))?;
            let wins = source == TitleSource::User
                || current
                    .title_source
                    .is_none_or(|old| old.rank() < source.rank());
            if !wins {
                return Ok(TitleWrite::Kept);
            }
            if let Some(title) = &title {
                ensure_unused(tx, title, Some(id))?;
            }
            tx.execute(
                "UPDATE sessions SET title = ?2, title_source = ?3 WHERE id = ?1",
                params![
                    id.to_string(),
                    title,
                    title.as_ref().map(|_| source.as_str())
                ],
            )
            .map_err(sql)?;
            Ok(TitleWrite::Written)
        })
    }

    /// Resolves a session reference: a full id, then a title (a bare base
    /// title picks the highest-numbered `#n` of its family), then an id
    /// prefix of at least [`MIN_PREFIX_CHARS`] characters (`ses_` optional).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `session.ambiguous` (listing the matches),
    /// `store.sqlite`.
    pub fn resolve_session(&self, reference: &str) -> Result<SessionId, Error> {
        let reference = reference.trim();
        self.read(|conn| {
            if let Ok(id) = SessionId::parse(reference)
                && sessions::load(conn, id)?.is_some()
            {
                return Ok(id);
            }
            if let Some(id) = by_title(conn, reference)? {
                return Ok(id);
            }
            by_prefix(conn, reference)
        })
    }
}

fn id_with_title(conn: &Connection, title: &str) -> Result<Option<SessionId>, Error> {
    conn.query_row("SELECT id FROM sessions WHERE title = ?1", [title], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .map_err(sql)?
    .map(|id| SessionId::parse(&id))
    .transpose()
}

fn by_title(conn: &Connection, reference: &str) -> Result<Option<SessionId>, Error> {
    let (base, _) = split_number(reference);
    if base != reference {
        // An explicit `#n` names exactly one session.
        return id_with_title(conn, reference);
    }
    let newest = family(conn, reference)?.into_iter().max_by_key(|(n, _)| *n);
    match newest {
        Some((_, title)) => id_with_title(conn, &title),
        None => Ok(None),
    }
}

fn by_prefix(conn: &Connection, reference: &str) -> Result<SessionId, Error> {
    let missing = || {
        Error::new(
            &codes::SESSION_NOT_FOUND,
            format!("no session matches '{reference}'"),
        )
        .with_hint("list sessions with `prometheus sessions list`")
    };
    if reference.chars().count() < MIN_PREFIX_CHARS || !reference.is_ascii() {
        return Err(missing());
    }
    let body = reference
        .get(..4)
        .filter(|head| head.eq_ignore_ascii_case("ses_"))
        .map_or(reference, |_| &reference[4..]);
    let prefix = format!("ses_{}", body.to_ascii_uppercase());
    let mut stmt = conn
        .prepare("SELECT id FROM sessions WHERE substr(id, 1, length(?1)) = ?1 ORDER BY id LIMIT 6")
        .map_err(sql)?;
    let ids = stmt
        .query_map([&prefix], |row| row.get::<_, String>(0))
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    match ids.as_slice() {
        [] => Err(missing()),
        [one] => SessionId::parse(one),
        many => Err(Error::new(
            &codes::SESSION_AMBIGUOUS,
            format!(
                "'{reference}' matches several sessions: {}{}",
                many[..many.len().min(5)].join(", "),
                if many.len() > 5 { ", ..." } else { "" }
            ),
        )
        .with_hint("give more characters of the id")),
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize, split_number};

    #[test]
    fn sanitising_strips_invisible_and_control_characters() {
        let raw = "  Tax\u{200B} prep\u{202E}\n\t2025\u{0007}  ";
        assert_eq!(sanitize(raw).unwrap().as_deref(), Some("Tax prep 2025"));
        assert_eq!(sanitize(" \u{FEFF}\u{2066} ").unwrap(), None);
    }

    #[test]
    fn too_long_title_is_refused_with_its_length() {
        let err = sanitize(&"x".repeat(101)).unwrap_err();
        assert_eq!(err.message(), "Title too long (101 chars, max 100)");
        assert!(
            sanitize(&"é".repeat(100)).unwrap().is_some(),
            "chars, not bytes"
        );
    }

    #[test]
    fn numbers_split_only_from_a_trailing_hash_suffix() {
        assert_eq!(split_number("Refactor #2"), ("Refactor", 2));
        assert_eq!(split_number("Refactor"), ("Refactor", 1));
        assert_eq!(
            split_number("Issue #12 follow-up"),
            ("Issue #12 follow-up", 1)
        );
        assert_eq!(split_number("Build #02"), ("Build #02", 1));
        assert_eq!(split_number(" #3"), (" #3", 1));
    }
}
