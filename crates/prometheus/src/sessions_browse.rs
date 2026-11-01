//! `prometheus sessions browse [--source <s>] [--all]` (spec:
//! session-storage, Interactive session browser): sessions newest first
//! with title, relative last activity, message count, source and a status
//! tag; typing text narrows the list (titles and full-text hits), a number
//! resumes that session in the chat, an empty line leaves. It needs a
//! terminal on stdin and stdout (exit 2 otherwise); prompts go to stderr.

use std::fmt::Write as _;
use std::io::{self, BufRead, Write};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::{EndReason, ListOptions, Role, SearchOptions, Session, Source, Store, Visibility};

use crate::chat::args::ChatArgs;
use crate::context::Context;
use crate::prompt::{stdin_is_tty, stdout_is_tty};

/// Rows shown at once.
const PAGE: usize = 20;

/// One listed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The session.
    pub id: SessionId,
    /// Its title (`(untitled)` when none).
    pub title: String,
    /// When it was last active.
    pub last_activity: Timestamp,
    /// Messages it holds.
    pub messages: u64,
    /// Its source (with the integration tag).
    pub source: String,
    /// `live`, `done`, `intr`, `err` or `empty`.
    pub tag: &'static str,
}

/// The status tag of `session` whose newest active message has `last`
/// role: `empty` without messages, `intr` / `err` for those end reasons,
/// `live` while it has not ended, else `done`.
#[must_use]
pub fn status_tag(session: &Session, last: Option<Role>) -> &'static str {
    match (last, session.end_reason) {
        (None, _) => "empty",
        (_, Some(EndReason::Interrupted)) => "intr",
        (_, Some(EndReason::Error)) => "err",
        (_, None) => "live",
        _ => "done",
    }
}

/// `3m`, `5h`, `2d` ago.
#[must_use]
pub fn relative(then: Timestamp, now: Timestamp) -> String {
    let seconds = now.duration_since(then).as_secs().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// What the browser lists.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// `--source`.
    pub source: Option<Source>,
    /// `--all`: hidden, archived and integration sessions too.
    pub all: bool,
    /// The typed query (titles and full-text hits).
    pub query: String,
}

/// The rows of `filter`, newest first.
///
/// # Errors
///
/// Store errors.
pub fn rows(store: &Store, filter: &Filter) -> Result<Vec<Row>, Error> {
    let sessions = store.list_sessions(&ListOptions {
        all: filter.all,
        sources: filter.source.into_iter().collect(),
        ..ListOptions::default()
    })?;
    let hits: Vec<SessionId> = if filter.query.trim().is_empty() {
        Vec::new()
    } else {
        let options = SearchOptions {
            limit: 200,
            ..SearchOptions::default()
        };
        store
            .search(&filter.query, &options)?
            .hits
            .into_iter()
            .map(|h| h.session_id)
            .collect()
    };
    let needle = filter.query.trim().to_lowercase();
    let mut rows = Vec::new();
    for session in sessions {
        let title = session
            .title
            .clone()
            .unwrap_or_else(|| "(untitled)".to_owned());
        if !needle.is_empty()
            && !title.to_lowercase().contains(&needle)
            && !hits.contains(&session.id)
        {
            continue;
        }
        let last = store
            .messages(session.id, Visibility::Active)?
            .last()
            .map(|m| m.role);
        rows.push(Row {
            id: session.id,
            tag: status_tag(&session, last),
            source: match &session.source_tag {
                Some(tag) => format!("{} ({tag})", session.source),
                None => session.source.to_string(),
            },
            messages: session.message_count,
            last_activity: session.last_activity_at,
            title,
        });
    }
    Ok(rows)
}

/// The listing of `rows` as the browser prints it.
#[must_use]
pub fn render(rows: &[Row], now: Timestamp) -> String {
    let mut text = String::new();
    for (n, row) in rows.iter().take(PAGE).enumerate() {
        let _ = writeln!(
            text,
            "{:>3}. [{:<5}] {:<40} {:>4} ago  {:>4} msgs  {}",
            n + 1,
            row.tag,
            row.title.chars().take(40).collect::<String>(),
            relative(row.last_activity, now),
            row.messages,
            row.source
        );
    }
    if rows.len() > PAGE {
        let _ = writeln!(text, "     … {} more; type to narrow", rows.len() - PAGE);
    }
    text
}

/// Runs the browser; a chosen session is resumed in the chat.
///
/// # Errors
///
/// `cli.requires_terminal` (exit 2) without a terminal; store errors.
pub fn run(
    source: Option<Source>,
    all: bool,
    ctx: &Context<'_>,
    ui: crate::chat::Ui,
) -> Result<u8, Error> {
    if !(stdin_is_tty() && stdout_is_tty()) {
        return Err(Error::new(
            &codes::CLI_REQUIRES_TERMINAL,
            "prometheus sessions browse needs a terminal; use prometheus sessions list",
        ));
    }
    let chosen = {
        let scope = ctx.scope()?;
        let store = Store::open(&scope)?;
        let mut filter = Filter {
            source,
            all,
            query: String::new(),
        };
        pick(
            &store,
            &mut filter,
            &mut io::stdin().lock(),
            &mut io::stderr(),
        )?
    };
    let Some(id) = chosen else {
        return Ok(0);
    };
    let args = ChatArgs {
        resume: Some(id.to_string()),
        ..ChatArgs::default()
    };
    crate::chat::run(&args, ctx, ui)
}

/// The pick loop: a number picks, text narrows, an empty line or end of
/// input leaves.
fn pick(
    store: &Store,
    filter: &mut Filter,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Option<SessionId>, Error> {
    let io = |e: io::Error| Error::new(&codes::OUTPUT_IO, format!("terminal: {e}"));
    loop {
        let listed = rows(store, filter)?;
        let heading = if filter.query.is_empty() {
            "sessions".to_owned()
        } else {
            format!("sessions matching '{}'", filter.query)
        };
        write!(out, "{heading}:\n{}", render(&listed, Timestamp::now())).map_err(io)?;
        write!(out, "number to resume, text to search, Enter to leave › ").map_err(io)?;
        out.flush().map_err(io)?;
        let mut line = String::new();
        if input.read_line(&mut line).map_err(io)? == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=listed.len().min(PAGE)).contains(&n) => {
                return Ok(Some(listed[n - 1].id));
            }
            Ok(_) => writeln!(out, "no row {line}").map_err(io)?,
            Err(_) => line.clone_into(&mut filter.query),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_use_the_largest_unit() {
        let now = Timestamp::from_second(1_000_000).unwrap();
        let ago = |s: i64| relative(Timestamp::from_second(1_000_000 - s).unwrap(), now);
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(180), "3m");
        assert_eq!(ago(7_200), "2h");
        assert_eq!(ago(3 * 86_400), "3d");
    }
}
