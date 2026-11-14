//! Notification subscriptions (spec: kanban, Notifications through the
//! owning profile). A subscription records where (adapter, chat, thread)
//! and as whom (the subscriber profile) to report a task's `completed`,
//! `blocked`, `gave_up` and `review_requested` events. The daemon
//! delivers them through that profile's own adapter; this module keeps
//! the rows, the per-subscription checkpoint (the last delivered event)
//! and the once-only warning flag.

use pmt_core::error::{Error, codes};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

use crate::board::{Board, event, load};
use crate::db::sql;
use crate::model::{Actor, Event};

/// The event kinds a subscription reports.
pub const NOTIFY_KINDS: [&str; 4] = ["completed", "blocked", "gave_up", "review_requested"];

/// One subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subscription {
    /// `s_<ULID>`.
    pub id: String,
    /// Its task.
    pub task_id: String,
    /// The adapter (`telegram`, `slack`...).
    pub adapter: String,
    /// The chat id.
    pub chat: String,
    /// The thread id.
    pub thread: Option<String>,
    /// `dm`, `group`, `channel` or `thread`.
    pub chat_type: Option<String>,
    /// `notify` (the only mode delivered by this build's notifier).
    pub delivery_mode: String,
    /// The profile whose adapter delivers.
    pub profile: String,
    /// Creation instant (Unix ms).
    pub created_at: i64,
    /// The last event delivered (or skipped).
    pub checkpoint: i64,
    /// Whether the unreachable-route warning was logged.
    pub warned: bool,
}

/// What subscribing names.
#[derive(Debug, Clone)]
pub struct NewSubscription {
    /// The adapter.
    pub adapter: String,
    /// The chat id.
    pub chat: String,
    /// The thread id.
    pub thread: Option<String>,
    /// The subscriber profile.
    pub profile: String,
}

fn sub_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: r.get(0)?,
        task_id: r.get(1)?,
        adapter: r.get(2)?,
        chat: r.get(3)?,
        thread: r.get(4)?,
        chat_type: r.get(5)?,
        delivery_mode: r.get(6)?,
        profile: r.get(7)?,
        created_at: r.get(8)?,
        checkpoint: r.get(9)?,
        warned: r.get::<_, i64>(10)? != 0,
    })
}

const SUB_COLUMNS: &str = "id, task_id, adapter, chat, thread, chat_type, delivery_mode, profile, created_at, checkpoint, warned";

impl Board {
    /// Subscribes a chat to a task; only events after now are reported.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`, `kanban.invalid_task` for a blank route.
    pub fn subscribe(
        &mut self,
        actor: &Actor,
        task: &str,
        new: &NewSubscription,
        now: i64,
    ) -> Result<Subscription, Error> {
        if new.adapter.trim().is_empty() || new.chat.trim().is_empty() {
            return Err(crate::model::invalid(
                "a subscription needs an adapter and a chat",
            ));
        }
        let label = actor.label();
        let id = crate::model::prefixed_id("s");
        self.write(|tx| {
            load(tx, task)?;
            let checkpoint: i64 = tx
                .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
                .map_err(sql)?;
            tx.execute(
                "INSERT INTO subscriptions(id, task_id, adapter, chat, thread, profile, created_at, checkpoint) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, task, new.adapter, new.chat, new.thread, new.profile, now, checkpoint],
            )
            .map_err(sql)?;
            event(tx, Some(task), None, &label, "subscribed", &json!({"subscription": id, "adapter": new.adapter, "profile": new.profile}), now)?;
            tx.query_row(&format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE id = ?1"), [&id], sub_row)
                .map_err(sql)
        })
    }

    /// Subscriptions of a task, or of the whole board.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn subscriptions(&self, task: Option<&str>) -> Result<Vec<Subscription>, Error> {
        let mut stmt = self
            .conn()
            .prepare(&format!(
                "SELECT {SUB_COLUMNS} FROM subscriptions WHERE (?1 IS NULL OR task_id = ?1) ORDER BY created_at, id"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map([task], sub_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// Removes a subscription.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found` naming the subscription.
    pub fn unsubscribe(&mut self, id: &str) -> Result<(), Error> {
        self.write(|tx| {
            let removed = tx
                .execute("DELETE FROM subscriptions WHERE id = ?1", [id])
                .map_err(sql)?;
            if removed == 0 {
                return Err(Error::new(
                    &codes::KANBAN_TASK_NOT_FOUND,
                    format!("subscription '{id}' not found"),
                ));
            }
            Ok(())
        })
    }

    /// Undelivered notifications, oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn pending_notifications(&self) -> Result<Vec<(Subscription, Event)>, Error> {
        let mut out = Vec::new();
        for sub in self.subscriptions(None)? {
            for event in self.events(Some(&sub.task_id))? {
                if event.id > sub.checkpoint && NOTIFY_KINDS.contains(&event.kind.as_str()) {
                    out.push((sub.clone(), event));
                }
            }
        }
        out.sort_by_key(|(_, e)| e.id);
        Ok(out)
    }

    /// Records that `sub` handled every event up to `event`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn checkpoint(&mut self, sub: &str, event: i64) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE subscriptions SET checkpoint = MAX(checkpoint, ?2) WHERE id = ?1",
                params![sub, event],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Records that `sub`'s unreachable-route warning was logged; `true`
    /// the first time.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn warn_once(&mut self, sub: &str) -> Result<bool, Error> {
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE subscriptions SET warned = 1 WHERE id = ?1 AND warned = 0",
                    [sub],
                )
                .map_err(sql)?;
            Ok(changed == 1)
        })
    }

    /// Purges subscriptions of tasks done for longer than `retention_ms`;
    /// returns how many.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn purge_done_subscriptions(
        &mut self,
        retention_ms: i64,
        now: i64,
    ) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM subscriptions WHERE task_id IN (SELECT id FROM tasks WHERE status = 'done' \
                 AND completed_at IS NOT NULL AND completed_at + ?1 <= ?2)",
                params![retention_ms, now],
            )
            .map_err(sql)
        })
    }

    /// Deletes board events of kind `tick` older than `before` (Unix ms);
    /// task events are never deleted.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn prune_ticks(&mut self, before: i64) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM events WHERE task_id IS NULL AND kind = 'tick' AND at < ?1",
                [before],
            )
            .map_err(sql)
        })
    }
}
