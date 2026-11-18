//! Ephemeral sessions (spec: workspace-architecture, Embeddable client
//! SDKs): a session an embedding program opens with `ephemeral = true` is
//! tagged [`TAG`] and hidden from listings, writes no trajectory, and its
//! transcript leaves the store when the session is unloaded or closed (and
//! at the next daemon start after a crash). The session row and its usage
//! ledger stay, so its spend still counts against every budget.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::Transaction;

use crate::error::sql;
use crate::store::Store;

/// The source tag of an ephemeral session.
pub const TAG: &str = "ephemeral";

/// The content tables of a session: everything but its row, its usage and
/// its bookkeeping.
const CONTENT: &[&str] = &[
    "DELETE FROM api_responses WHERE session_id = ?1",
    "DELETE FROM session_todos WHERE session_id = ?1",
    "DELETE FROM turn_journal WHERE session_id = ?1",
    "DELETE FROM session_prefill WHERE session_id = ?1",
    "DELETE FROM session_pins WHERE session_id = ?1",
    "DELETE FROM session_prompts WHERE session_id = ?1",
    "DELETE FROM session_compression WHERE session_id = ?1",
    "DELETE FROM messages WHERE session_id = ?1",
    "DELETE FROM blob_refs WHERE owner_kind = 'session' AND owner_id = ?1",
    "DELETE FROM blob_refs WHERE owner_kind = 'turn_journal' AND owner_id = ?1",
    "UPDATE sessions SET title = NULL, lessons = NULL, message_count = 0 WHERE id = ?1",
];

fn forget(tx: &Transaction<'_>, session: &str) -> Result<(), Error> {
    // The deleted rows are zeroed where they lie. Older copies a row left
    // in a page's free space before (an update, a page split) stay until
    // SQLite reuses that space, as for any deleted session.
    tx.pragma_update(None, "secure_delete", true).map_err(sql)?;
    crate::index::remove_session(tx, session)?;
    for statement in CONTENT {
        tx.execute(statement, [session]).map_err(sql)?;
    }
    tx.pragma_update(None, "secure_delete", false)
        .map_err(sql)?;
    Ok(())
}

impl Store {
    /// Removes the transcript of session `id` (messages, prompts, journal,
    /// search index), keeping its row and usage.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.io`, `store.sqlite`.
    pub fn forget_transcript(&self, id: SessionId) -> Result<(), Error> {
        let session = id.to_string();
        self.write(|tx| forget(tx, &session))
    }

    /// Removes the transcripts every ephemeral session left behind (a
    /// process that stopped before unloading them); returns how many.
    ///
    /// # Errors
    ///
    /// As [`Store::forget_transcript`].
    pub fn forget_ephemeral(&self) -> Result<usize, Error> {
        self.write(|tx| {
            let left: Vec<String> = {
                let mut statement = tx
                    .prepare(
                        "SELECT s.id FROM sessions s WHERE s.source_tag = ?1
                         AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id)",
                    )
                    .map_err(sql)?;
                statement
                    .query_map([TAG], |row| row.get(0))
                    .map_err(sql)?
                    .collect::<Result<_, _>>()
                    .map_err(sql)?
            };
            for session in &left {
                forget(tx, session)?;
            }
            Ok(left.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TAG;
    use crate::messages::{NewMessage, Visibility};
    use crate::model::Role;
    use crate::{NewSession, Source, Store};

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let home = pmt_config::Home::open(&root.path().join("home")).unwrap();
        let dir = home
            .profile(&pmt_config::ProfileName::parse("default").unwrap())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            dir,
            &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        (root, Store::open(&scope).unwrap())
    }

    fn session(store: &Store, tag: Option<&str>) -> pmt_core::id::SessionId {
        let mut new = NewSession::new(Source::Cli);
        new.source_tag = tag.map(str::to_owned);
        let id = store.create_session(&new).unwrap().id;
        store
            .append_message(id, &NewMessage::new(Role::User, "a secret plan"))
            .unwrap();
        id
    }

    #[test]
    fn a_forgotten_transcript_leaves_the_session_row_but_no_message() {
        let (_dir, store) = store();
        let id = session(&store, Some(TAG));
        store.forget_transcript(id).unwrap();
        assert!(store.messages(id, Visibility::All).unwrap().is_empty());
        assert!(store.session(id).is_ok(), "the row (and its usage) stays");
    }

    #[test]
    fn only_ephemeral_sessions_are_swept() {
        let (_dir, store) = store();
        let kept = session(&store, None);
        let left = session(&store, Some(TAG));
        assert_eq!(store.forget_ephemeral().unwrap(), 1);
        assert!(store.messages(left, Visibility::All).unwrap().is_empty());
        assert_eq!(store.messages(kept, Visibility::All).unwrap().len(), 1);
        assert_eq!(store.forget_ephemeral().unwrap(), 0);
    }
}
