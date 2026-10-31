//! What every background review shares (skills and memory): a hidden child
//! session to run in and an excerpt of the reviewed conversation.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::{LineageKind, NewSession, Role, Source, Store, Visibility};

/// Most characters of conversation given to a review.
pub const EXCERPT_CHARS: usize = 24_000;

/// The newest user and assistant text of `session`, oldest first, within
/// [`EXCERPT_CHARS`].
///
/// # Errors
///
/// Store errors.
pub fn excerpt(store: &Store, session: SessionId) -> Result<String, Error> {
    let rows = store.messages(session, Visibility::Active)?;
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0;
    for row in rows.iter().rev() {
        let label = match row.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            _ => continue,
        };
        let Some(text) = row.content.as_deref().filter(|t| !t.trim().is_empty()) else {
            continue;
        };
        used += text.len();
        if used > EXCERPT_CHARS && !parts.is_empty() {
            break;
        }
        parts.push(format!("{label}: {text}"));
    }
    parts.reverse();
    Ok(parts.join("\n\n"))
}

/// Creates a review's hidden child session (tagged `tool`, so listings and
/// continue pickers skip it).
///
/// # Errors
///
/// Store errors.
pub fn child_session(store: &Store, parent: SessionId) -> Result<SessionId, Error> {
    let mut new = NewSession::child(Source::Subagent, parent, LineageKind::Delegate);
    new.source_tag = Some("tool".to_owned());
    Ok(store.create_session(&new)?.id)
}
