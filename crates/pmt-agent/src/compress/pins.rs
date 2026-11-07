//! `/pin` and `/unpin` (spec: context-compression, Pinned messages survive
//! compaction).
//!
//! A pin captures a user or assistant message's text; every compaction
//! copies the pinned texts verbatim into the summary's `## Pinned` section,
//! so they survive any number of compactions. The pinned texts together
//! stay within `compression.pinned_max_tokens`.

use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{Message as Row, Role, Store, Visibility};

use super::{is_real_user, is_summary, tokens};

/// A pinned message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// The message.
    pub id: MessageId,
    /// Its text.
    pub text: String,
}

/// Pins `target` (`last`, empty for `last`, or a message id or unique id
/// prefix) of `session`, refusing a pin that would make the pinned texts
/// exceed `max_tokens`.
///
/// # Errors
///
/// `compression.pin_unknown` for no such message, `compression.pin_limit`
/// over the budget; store failures.
pub fn pin(
    store: &Store,
    session: SessionId,
    target: &str,
    max_tokens: u64,
) -> Result<Pinned, Error> {
    let rows = store.messages(session, Visibility::Active)?;
    let candidates: Vec<&Row> = rows.iter().filter(|r| pinnable(r)).collect();
    let target = target.trim();
    let row = if target.is_empty() || target == "last" {
        candidates.last().copied()
    } else {
        by_id(&candidates, target)?
    }
    .ok_or_else(|| unknown(target))?;
    let text = row.content.clone().unwrap_or_default();
    let others: u64 = store
        .pins(session)?
        .iter()
        .filter(|p| p.message_id != row.id)
        .map(|p| tokens(&p.text))
        .sum();
    if others + tokens(&text) > max_tokens {
        return Err(Error::new(
            &codes::COMPRESSION_PIN_LIMIT,
            format!("pinned content would exceed {max_tokens} tokens; unpin something first"),
        ));
    }
    store.pin(session, row.id, &text)?;
    Ok(Pinned { id: row.id, text })
}

/// Unpins the pinned message `target` (an id or unique id prefix).
///
/// # Errors
///
/// `compression.pin_unknown` when no pin matches; store failures.
pub fn unpin(store: &Store, session: SessionId, target: &str) -> Result<MessageId, Error> {
    let target = target.trim();
    let matching: Vec<MessageId> = store
        .pins(session)?
        .into_iter()
        .map(|p| p.message_id)
        .filter(|id| !target.is_empty() && id.to_string().starts_with(target))
        .collect();
    match matching.as_slice() {
        [id] => {
            store.unpin(session, *id)?;
            Ok(*id)
        }
        _ => Err(unknown(target)),
    }
}

/// The `## Pinned` body of `session`: each pinned text verbatim.
///
/// # Errors
///
/// Store failures.
pub fn section(store: &Store, session: SessionId) -> Result<Option<String>, Error> {
    let texts: Vec<String> = store.pins(session)?.into_iter().map(|p| p.text).collect();
    Ok((!texts.is_empty()).then(|| texts.join("\n\n")))
}

/// A user message or an assistant reply with text (not a summary, not a
/// notice Prometheus added).
fn pinnable(row: &Row) -> bool {
    let text = row.content.as_deref().is_some_and(|t| !t.trim().is_empty());
    let role = is_real_user(row) || (row.role == Role::Assistant && !is_summary(row));
    text && role
}

fn by_id<'a>(candidates: &[&'a Row], target: &str) -> Result<Option<&'a Row>, Error> {
    let matching: Vec<&Row> = candidates
        .iter()
        .copied()
        .filter(|r| r.id.to_string().starts_with(target))
        .collect();
    match matching.as_slice() {
        [row] => Ok(Some(row)),
        [] => Ok(None),
        _ => Err(Error::new(
            &codes::COMPRESSION_PIN_UNKNOWN,
            format!("'{target}' matches more than one message; give more of the id"),
        )),
    }
}

fn unknown(target: &str) -> Error {
    let what = if target.is_empty() { "last" } else { target };
    Error::new(
        &codes::COMPRESSION_PIN_UNKNOWN,
        format!("no message '{what}' to pin or unpin in this session"),
    )
}
