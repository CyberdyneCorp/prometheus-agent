//! Notification subscriptions (spec: kanban, Notifications through the
//! owning profile; Subscription delivery modes and inheritance). A
//! subscription records where (adapter, chat, thread, chat type), as whom
//! (the subscriber profile) and how (`notify`, `notify+wake`, `wake`) to
//! report a task's events. The daemon delivers passive messages through
//! that profile's own adapter and wakes the subscriber's agent in the
//! chat's session; this module keeps the rows, one checkpoint per leg
//! (passive message, wake) so a wake retried later never re-sends a
//! delivered message, and the once-only warning flag.

use pmt_core::error::{Error, codes};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

use crate::board::{Board, event, load};
use crate::db::sql;
use crate::model::{Actor, Event, invalid};

/// The event kinds a passive message reports.
pub const NOTIFY_KINDS: [&str; 4] = ["completed", "blocked", "gave_up", "review_requested"];

/// The event kinds that wake the subscriber's agent (status, archived and
/// unblocked events never do).
pub const WAKE_KINDS: [&str; 7] = [
    "completed",
    "blocked",
    "gave_up",
    "crashed",
    "timed_out",
    "review_requested",
    "block_loop_detected",
];

/// How a subscription reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DeliveryMode {
    /// A passive message only.
    #[serde(rename = "notify")]
    Notify,
    /// A passive message and a turn of the subscriber's agent.
    #[serde(rename = "notify+wake")]
    NotifyWake,
    /// A turn only.
    #[serde(rename = "wake")]
    Wake,
}

impl DeliveryMode {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::NotifyWake => "notify+wake",
            Self::Wake => "wake",
        }
    }

    /// Parses `notify`, `notify+wake` or `wake`.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        match text {
            "notify" => Ok(Self::Notify),
            "notify+wake" => Ok(Self::NotifyWake),
            "wake" => Ok(Self::Wake),
            other => Err(invalid(format!(
                "unknown delivery mode '{other}' (expected notify|notify+wake|wake)"
            ))),
        }
    }

    const fn notifies(self) -> bool {
        matches!(self, Self::Notify | Self::NotifyWake)
    }

    const fn wakes(self) -> bool {
        matches!(self, Self::Wake | Self::NotifyWake)
    }
}

/// The chat types a wake resolves the chat's session key with.
pub const CHAT_TYPES: [&str; 4] = ["dm", "group", "channel", "thread"];

/// Which half of a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// The passive message.
    Notify,
    /// The subscriber agent's turn.
    Wake,
}

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
    /// `notify`, `notify+wake` or `wake`.
    pub delivery_mode: String,
    /// The profile whose adapter delivers.
    pub profile: String,
    /// Creation instant (Unix ms).
    pub created_at: i64,
    /// The last event whose passive message was delivered (or skipped).
    pub checkpoint: i64,
    /// The last event whose wake was admitted (or skipped).
    pub wake_checkpoint: i64,
    /// The chat's session, when subscribed from inside it.
    pub session_id: Option<String>,
    /// Whether the unreachable-route warning was logged.
    pub warned: bool,
}

impl Subscription {
    /// Its delivery mode (an unknown stored mode notifies).
    #[must_use]
    pub fn mode(&self) -> DeliveryMode {
        DeliveryMode::parse(&self.delivery_mode).unwrap_or(DeliveryMode::Notify)
    }
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
    /// `dm`, `group`, `channel` or `thread`.
    pub chat_type: Option<String>,
    /// How it reports.
    pub mode: DeliveryMode,
    /// The chat's session, when known.
    pub session_id: Option<String>,
}

impl NewSubscription {
    /// A passive subscription of `profile` to a chat.
    #[must_use]
    pub fn notify(adapter: &str, chat: &str, thread: Option<&str>, profile: &str) -> Self {
        Self {
            adapter: adapter.to_owned(),
            chat: chat.to_owned(),
            thread: thread.map(str::to_owned),
            profile: profile.to_owned(),
            chat_type: None,
            mode: DeliveryMode::Notify,
            session_id: None,
        }
    }
}

/// One undelivered half of a notification.
#[derive(Debug, Clone)]
pub struct Pending {
    /// The subscription.
    pub sub: Subscription,
    /// The event.
    pub event: Event,
    /// Message or wake.
    pub leg: Leg,
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
        wake_checkpoint: r.get(10)?,
        session_id: r.get(11)?,
        warned: r.get::<_, i64>(12)? != 0,
    })
}

const SUB_COLUMNS: &str = "id, task_id, adapter, chat, thread, chat_type, delivery_mode, profile, \
    created_at, checkpoint, wake_checkpoint, session_id, warned";

/// Copies the subscriptions of `run`'s task to `task` (a task a worker
/// created), modes unchanged; only events after now are reported.
pub(crate) fn inherit(
    tx: &rusqlite::Transaction<'_>,
    run: &str,
    task: &str,
    now: i64,
) -> Result<(), Error> {
    let checkpoint: i64 = tx
        .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
        .map_err(sql)?;
    let mut stmt = tx
        .prepare(&format!(
            "SELECT {SUB_COLUMNS} FROM subscriptions WHERE task_id = \
             (SELECT task_id FROM task_runs WHERE id = ?1)"
        ))
        .map_err(sql)?;
    let subs = stmt
        .query_map([run], sub_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    for sub in subs {
        tx.execute(
            "INSERT INTO subscriptions(id, task_id, adapter, chat, thread, chat_type, delivery_mode, \
             profile, created_at, checkpoint, wake_checkpoint, session_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
            params![
                crate::model::prefixed_id("s"),
                task,
                sub.adapter,
                sub.chat,
                sub.thread,
                sub.chat_type,
                sub.delivery_mode,
                sub.profile,
                now,
                checkpoint,
                sub.session_id
            ],
        )
        .map_err(sql)?;
    }
    Ok(())
}

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
            return Err(invalid("a subscription needs an adapter and a chat"));
        }
        if let Some(kind) = new.chat_type.as_deref().filter(|k| !CHAT_TYPES.contains(k)) {
            return Err(invalid(format!(
                "unknown chat type '{kind}' (expected dm|group|channel|thread)"
            )));
        }
        let label = actor.label();
        let id = crate::model::prefixed_id("s");
        self.write(|tx| {
            load(tx, task)?;
            let checkpoint: i64 = tx
                .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
                .map_err(sql)?;
            tx.execute(
                "INSERT INTO subscriptions(id, task_id, adapter, chat, thread, chat_type, delivery_mode, \
                 profile, created_at, checkpoint, wake_checkpoint, session_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
                params![
                    id,
                    task,
                    new.adapter,
                    new.chat,
                    new.thread,
                    new.chat_type,
                    new.mode.as_str(),
                    new.profile,
                    now,
                    checkpoint,
                    new.session_id
                ],
            )
            .map_err(sql)?;
            event(tx, Some(task), None, &label, "subscribed", &json!({"subscription": id, "adapter": new.adapter, "profile": new.profile, "delivery_mode": new.mode.as_str()}), now)?;
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

    /// Undelivered messages and wakes, oldest event first (a message
    /// before its wake).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn pending_notifications(&self) -> Result<Vec<Pending>, Error> {
        let mut out = Vec::new();
        for sub in self.subscriptions(None)? {
            let mode = sub.mode();
            for event in self.events(Some(&sub.task_id))? {
                let kind = event.kind.as_str();
                if mode.notifies() && event.id > sub.checkpoint && NOTIFY_KINDS.contains(&kind) {
                    out.push(Pending {
                        sub: sub.clone(),
                        event: event.clone(),
                        leg: Leg::Notify,
                    });
                }
                if mode.wakes() && event.id > sub.wake_checkpoint && WAKE_KINDS.contains(&kind) {
                    out.push(Pending {
                        sub: sub.clone(),
                        event,
                        leg: Leg::Wake,
                    });
                }
            }
        }
        out.sort_by_key(|p| (p.event.id, p.leg == Leg::Wake));
        Ok(out)
    }

    /// Records that `sub` handled `leg` of every event up to `event`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn checkpoint(&mut self, sub: &str, leg: Leg, event: i64) -> Result<(), Error> {
        let column = match leg {
            Leg::Notify => "checkpoint",
            Leg::Wake => "wake_checkpoint",
        };
        self.write(|tx| {
            tx.execute(
                &format!("UPDATE subscriptions SET {column} = MAX({column}, ?2) WHERE id = ?1"),
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
