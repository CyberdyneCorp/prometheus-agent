//! Which stored session a chat continues (spec: cli, Resume and continue
//! resolution): `-r/--resume <ref>` by id, id prefix, title or `latest`,
//! `-c` for the newest CLI session of the directory (preferring the one
//! last used from this terminal) and `-c <name>` for the newest session of
//! a titled lineage.

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;

/// What the flags ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request<'a> {
    /// A new session.
    New,
    /// `-r/--resume <ref>`.
    Resume(&'a str),
    /// `-c` (empty) or `-c <name>`.
    Continue(&'a str),
}

impl<'a> Request<'a> {
    /// The request of `-r` / `-c`.
    #[must_use]
    pub fn of(resume: Option<&'a str>, continue_: Option<&'a str>) -> Self {
        match (resume, continue_) {
            (Some(reference), _) => Self::Resume(reference),
            (None, Some(name)) => Self::Continue(name),
            (None, None) => Self::New,
        }
    }
}

/// Where the lookup happens.
#[derive(Debug, Clone, Copy)]
pub struct Place<'a> {
    /// The session's working directory (`--cwd` or the launch directory).
    pub cwd: &'a Path,
    /// This terminal's identity, when `sessions.terminal_continue` is on.
    pub terminal: Option<&'a str>,
}

/// The session to continue, or `None` for a new one.
///
/// # Errors
///
/// `chat.nothing_to_continue` (66) with "no session to continue in
/// <dir>", `session.not_found` (66), `session.ambiguous` (2),
/// `chat.import_unavailable` (69) for `@claude-code`/`@codex`.
pub fn resolve(
    store: &Store,
    request: &Request<'_>,
    place: Place<'_>,
) -> Result<Option<SessionId>, Error> {
    let cwd = place.cwd.to_string_lossy();
    let nothing = || {
        Error::new(
            &codes::CHAT_NOTHING_TO_CONTINUE,
            format!("no session to continue in {}", place.cwd.display()),
        )
    };
    match request {
        Request::New => Ok(None),
        Request::Resume(reference) if reference.starts_with('@') => Err(Error::new(
            &codes::CHAT_IMPORT_UNAVAILABLE,
            format!(
                "resuming '{reference}' imports another agent's log, which this build cannot do yet"
            ),
        )),
        Request::Resume("latest") => store
            .continue_target(&cwd)?
            .map(|s| Some(s.id))
            .ok_or_else(nothing),
        Request::Resume(reference) => Ok(Some(store.resolve_session(reference)?)),
        Request::Continue("") => store
            .continue_target_from(&cwd, place.terminal)?
            .map(|s| Some(s.id))
            .ok_or_else(nothing),
        Request::Continue(name) => match store.resolve_session(name) {
            Ok(id) => Ok(Some(id)),
            Err(err) if err.code() == &codes::SESSION_NOT_FOUND => Err(nothing()),
            Err(err) => Err(err),
        },
    }
}
