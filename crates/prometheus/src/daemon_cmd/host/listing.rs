//! `session.list` in the daemon: the profile's sessions newest activity
//! first, optionally narrowed the way the ACP adapter's session catalog
//! needs (spec: acp-adapter, Session list is a paged read-only catalog):
//! by creating client, working directory and message count, one row per
//! conversation (a session continued after compression is listed as its
//! newest continuation), in pages whose cursor is an opaque token. A
//! cursor this daemon did not hand out is refused, never read as "the
//! first page".

use std::sync::atomic::Ordering;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_rpc::contracts::{SessionList, SessionListParams, SessionSummary};

use super::{Profile, source_of};

/// Where a page ends: the last row's activity and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    activity_ms: i64,
    id: SessionId,
}

impl Cursor {
    fn encode(self) -> String {
        URL_SAFE_NO_PAD.encode(format!("v1:{}:{}", self.activity_ms, self.id))
    }

    fn decode(text: &str) -> Result<Self, Error> {
        let invalid = || Error::new(&codes::RPC_INVALID_PARAMS, "invalid cursor");
        let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
        let text = String::from_utf8(bytes).map_err(|_| invalid())?;
        let mut parts = text.splitn(3, ':');
        let (Some("v1"), Some(ms), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(invalid());
        };
        Ok(Self {
            activity_ms: ms.parse().map_err(|_| invalid())?,
            id: id.parse().map_err(|_| invalid())?,
        })
    }

    fn of(session: &pmt_store::Session) -> Self {
        Self {
            activity_ms: session.last_activity_at.as_millisecond(),
            id: session.id,
        }
    }
}

/// Whether `params` narrows the listing beyond the plain newest-first
/// page.
fn narrowed(params: &SessionListParams) -> bool {
    params.source.is_some()
        || params.cwd.is_some()
        || params.min_messages.is_some()
        || params.tips_only
        || params.cursor.is_some()
}

/// The sessions `params` asks for.
pub(super) async fn list(
    profile: &Profile,
    params: SessionListParams,
) -> Result<SessionList, Error> {
    let page = usize::try_from(params.limit.unwrap_or(50).max(1)).unwrap_or(50);
    let after = params.cursor.as_deref().map(Cursor::decode).transpose()?;
    let options = pmt_store::ListOptions {
        limit: (!narrowed(&params)).then_some(params.limit.unwrap_or(50)),
        all: params.include_hidden,
        sources: params
            .source
            .map(|s| vec![source_of(Some(s))])
            .unwrap_or_default(),
        ..pmt_store::ListOptions::default()
    };
    let store = &profile.store;
    let mut rows = store
        .list_sessions(&options)?
        .into_iter()
        // The canonical Bot Chat is reached through its bot, not the
        // sessions list (spec: desktop-app, Bot chats are created
        // atomically by the daemon).
        .filter(|s| params.include_hidden || !s.bot_chat)
        .filter(|s| params.cwd.is_none() || s.cwd == params.cwd)
        .filter(|s| params.min_messages.is_none_or(|min| s.message_count >= min))
        .filter(|s| !params.tips_only || store.compression_tip(s.id).is_ok_and(|tip| tip == s.id))
        .filter(|s| after.is_none_or(|after| Cursor::of(s) < after));
    let mut taken: Vec<pmt_store::Session> = rows.by_ref().take(page).collect();
    let next_cursor = if rows.next().is_some() {
        taken.last().map(|s| Cursor::of(s).encode())
    } else {
        None
    };
    let loaded = profile.loaded.lock().await;
    let sessions = taken
        .drain(..)
        .map(|s| SessionSummary {
            live: loaded
                .get(&s.id)
                .is_some_and(|l| l.busy.load(Ordering::Acquire)),
            session_id: s.id.to_string(),
            title: s.title,
            started_at: s.started_at.to_string(),
            project_id: s.project_id.map(|p| p.to_string()),
            cwd: s.cwd,
            updated_at: Some(s.last_activity_at.to_string()),
        })
        .collect();
    Ok(SessionList {
        sessions,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn a_cursor_round_trips_and_a_forged_one_is_refused() {
        let cursor = Cursor {
            activity_ms: 1_700_000_000_000,
            id: "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W".parse().unwrap(),
        };
        assert_eq!(Cursor::decode(&cursor.encode()).unwrap(), cursor);
        for forged in ["abc", "", "djE6eDp5"] {
            let err = Cursor::decode(forged).unwrap_err();
            assert_eq!(err.message(), "invalid cursor", "{forged}");
        }
    }
}
