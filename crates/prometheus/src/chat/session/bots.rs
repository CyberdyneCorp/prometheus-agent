//! What makes a session a bot's canonical Bot Chat (spec: bot-mode,
//! Canonical Bot Chat; Bot protocol in canonical Bot Chats; `message_agent`
//! tool; Bot Mode off switch): only the session flag `bot_chat` decides,
//! so a headless Bot Chat behaves as the desktop's. With Bot Mode and its
//! protocol on, it gets `message_agent` and the protocol section in its
//! frozen prompt; every session gets the turn notices of @mentions.

use std::sync::Arc;

use pmt_agent::turn::TurnNotes;
use pmt_config::ProfileScope;
use pmt_store::Store;
use pmt_tools::agent_level_bots::MESSAGE_AGENT;
use pmt_tools::{Selection, ToolRegistry};

use crate::bot_host::protocol;

/// Whether `session` is a canonical Bot Chat with the bot protocol on.
#[must_use]
pub fn speaks_protocol(scope: &ProfileScope, session: &pmt_store::Session) -> bool {
    let bot_mode = &scope.config().bot_mode;
    session.bot_chat && bot_mode.enabled && bot_mode.protocol
}

/// `base` with `message_agent` for a Bot Chat that speaks the protocol.
#[must_use]
pub fn selection(
    registry: &ToolRegistry,
    scope: &ProfileScope,
    session: &pmt_store::Session,
    base: Selection,
) -> Selection {
    if !speaks_protocol(scope, session) || registry.get(MESSAGE_AGENT).is_err() {
        return base;
    }
    let mut selection = base;
    if !selection.tools.iter().any(|t| t == MESSAGE_AGENT) {
        selection.tools.push(MESSAGE_AGENT.to_owned());
    }
    selection.disabled.remove(MESSAGE_AGENT);
    selection
}

/// The protocol section of session `id`'s frozen prompt, when it is a Bot
/// Chat that speaks the protocol.
#[must_use]
pub fn section(scope: &ProfileScope, store: &Store, id: pmt_core::id::SessionId) -> Option<String> {
    let session = store.session(id).ok()?;
    speaks_protocol(scope, &session).then(|| protocol::section(scope, store, &id.to_string()))
}

/// The turn notices of session `id`.
#[must_use]
pub fn notes(
    scope: &ProfileScope,
    store: &Arc<Store>,
    id: pmt_core::id::SessionId,
) -> Option<Arc<dyn TurnNotes>> {
    let session = store.session(id).ok()?;
    // A room session's turns quote the room: its mentions are the room's.
    if session
        .source_tag
        .as_deref()
        .is_some_and(|t| t.starts_with("room-"))
    {
        return None;
    }
    Some(protocol::notes(
        scope,
        store,
        speaks_protocol(scope, &session),
    ))
}

/// Whether stored session `session` of `scope` is a canonical Bot Chat
/// (`/new` and `/reset` compress it instead of starting a new session).
#[must_use]
pub fn is_bot_chat(scope: &ProfileScope, session: &str) -> bool {
    let Ok(id) = session.parse::<pmt_core::id::SessionId>() else {
        return false;
    };
    Store::open(scope)
        .and_then(|store| store.session(id))
        .is_ok_and(|row| row.bot_chat)
}

/// The profile's canonical Bot Chat, created when it has none: the same
/// get-or-create transaction as `bot.chat.ensure` (`chat --bot-chat`).
///
/// # Errors
///
/// Store errors.
pub fn ensure_bot_chat(scope: &ProfileScope) -> Result<String, pmt_core::error::Error> {
    let store = Store::open(scope)?;
    let cwd = scope.dir().join("workspace");
    std::fs::create_dir_all(&cwd)
        .map_err(|e| pmt_core::error::Error::io(&pmt_core::error::codes::HOME_IO, &cwd, &e))?;
    let mut new = pmt_store::NewSession::new(pmt_store::Source::Desktop);
    new.cwd = Some(cwd.to_string_lossy().into_owned());
    let (session, _) = store.ensure_bot_chat(&new)?;
    Ok(session.id.to_string())
}
