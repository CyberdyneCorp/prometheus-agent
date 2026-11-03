//! Choices made from a chat that outlive the daemon (spec: messaging-
//! gateway, Adapter operations from chat; Runtime metadata footer;
//! Interactive model picker and chat model override): adapters an admin
//! paused, and per-chat preferences (the footer toggle, the model new
//! sessions of the chat use).

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use super::{millis, timestamp};
use crate::error::sql;
use crate::store::Store;

/// A manual pause of an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterPause {
    /// Platform name.
    pub platform: String,
    /// When it was paused.
    pub paused_at: Timestamp,
    /// Who paused it (`<platform>:<sender>`, `cli`).
    pub actor: String,
}

/// A chat's stored preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatPrefs {
    /// `/footer on|off`; `None` follows the display policy.
    pub footer: Option<bool>,
    /// `/model` override for new sessions (`provider:model`).
    pub model: Option<String>,
}

impl Store {
    /// Pauses the profile's `platform` adapter; `false` when it already
    /// was.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pause_adapter(&self, platform: &str, actor: &str) -> Result<bool, Error> {
        self.write(|tx| {
            let changed = tx
                .execute(
                    "INSERT OR IGNORE INTO gateway_adapter_pauses (platform, paused_at, actor) \
                     VALUES (?1, ?2, ?3)",
                    params![platform, millis(Timestamp::now()), actor],
                )
                .map_err(sql)?;
            Ok(changed > 0)
        })
    }

    /// Clears a manual pause; `false` when there was none.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn resume_adapter(&self, platform: &str) -> Result<bool, Error> {
        self.write(|tx| {
            let changed = tx
                .execute(
                    "DELETE FROM gateway_adapter_pauses WHERE platform = ?1",
                    [platform],
                )
                .map_err(sql)?;
            Ok(changed > 0)
        })
    }

    /// The manual pause of `platform`, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn adapter_pause(&self, platform: &str) -> Result<Option<AdapterPause>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT platform, paused_at, actor FROM gateway_adapter_pauses \
                 WHERE platform = ?1",
                [platform],
                |row| {
                    Ok(AdapterPause {
                        platform: row.get(0)?,
                        paused_at: timestamp(row.get(1)?),
                        actor: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
        })
    }

    /// The preferences stored for a chat.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn chat_prefs(&self, platform: &str, chat_id: &str) -> Result<ChatPrefs, Error> {
        let found = self.read(|conn| {
            conn.query_row(
                "SELECT footer, model FROM gateway_chat_prefs \
                 WHERE platform = ?1 AND chat_id = ?2",
                [platform, chat_id],
                |row| {
                    Ok(ChatPrefs {
                        footer: row.get::<_, Option<i64>>(0)?.map(|f| f != 0),
                        model: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
        })?;
        Ok(found.unwrap_or_default())
    }

    /// Stores a chat's footer toggle (`None` follows the display policy).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_chat_footer(
        &self,
        platform: &str,
        chat_id: &str,
        footer: Option<bool>,
    ) -> Result<(), Error> {
        self.upsert_pref(platform, chat_id, "footer", &footer.map(i64::from).into())
    }

    /// Stores a chat's model override (`None` clears it).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_chat_model(
        &self,
        platform: &str,
        chat_id: &str,
        model: Option<&str>,
    ) -> Result<(), Error> {
        let value = model.map_or(rusqlite::types::Value::Null, |m| {
            rusqlite::types::Value::Text(m.to_owned())
        });
        self.upsert_pref(platform, chat_id, "model", &value)
    }

    fn upsert_pref(
        &self,
        platform: &str,
        chat_id: &str,
        column: &'static str,
        value: &rusqlite::types::Value,
    ) -> Result<(), Error> {
        // `column` is one of two literals above, never input.
        let statement = format!(
            "INSERT INTO gateway_chat_prefs (platform, chat_id, {column}, updated_at) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (platform, chat_id) \
             DO UPDATE SET {column} = excluded.{column}, updated_at = excluded.updated_at"
        );
        self.write(|tx| {
            tx.execute(
                &statement,
                params![platform, chat_id, value, millis(Timestamp::now())],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

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
        let store = Store::open(&scope).unwrap();
        (root, store)
    }

    #[test]
    fn a_pause_is_kept_until_resumed() {
        let (_dir, store) = store();
        assert!(store.pause_adapter("discord", "telegram:1").unwrap());
        assert!(!store.pause_adapter("discord", "cli").unwrap());
        let pause = store.adapter_pause("discord").unwrap().unwrap();
        assert_eq!(pause.actor, "telegram:1");
        assert!(store.resume_adapter("discord").unwrap());
        assert!(store.adapter_pause("discord").unwrap().is_none());
        assert!(!store.resume_adapter("discord").unwrap());
    }

    #[test]
    fn chat_preferences_are_set_independently() {
        let (_dir, store) = store();
        store
            .set_chat_model("telegram", "42", Some("anthropic:claude-sonnet-4.5"))
            .unwrap();
        store.set_chat_footer("telegram", "42", Some(true)).unwrap();
        let prefs = store.chat_prefs("telegram", "42").unwrap();
        assert_eq!(prefs.footer, Some(true));
        assert_eq!(prefs.model.as_deref(), Some("anthropic:claude-sonnet-4.5"));
        store.set_chat_model("telegram", "42", None).unwrap();
        let prefs = store.chat_prefs("telegram", "42").unwrap();
        assert_eq!((prefs.footer, prefs.model), (Some(true), None));
        assert_eq!(
            store.chat_prefs("slack", "x").unwrap(),
            super::ChatPrefs::default()
        );
    }
}
